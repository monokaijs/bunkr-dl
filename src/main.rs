use base64::Engine;
use eframe::egui;
use futures_util::StreamExt;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, ORIGIN, REFERER, USER_AGENT};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::AsyncWriteExt;

const API_URLS: &[&str] = &[
  "https://get.bunkrr.su/api/_001_v2",
  "https://apidl.bunkr.ru/api/_001_v2",
];
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36";
const CONCURRENT_DOWNLOADS: usize = 3;

#[derive(Debug, Clone, Deserialize)]
struct AlbumFile {
  id: u64,
  original: String,
  #[serde(default)]
  size: u64,
  #[serde(default)]
  timestamp: String,
  #[serde(default)]
  extension: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
  encrypted: bool,
  timestamp: u64,
  url: String,
}

#[derive(Debug, Clone, PartialEq)]
enum FileStatus {
  Idle,
  Resolving,
  Downloading,
  Paused,
  Done,
  Failed(String),
}

#[derive(Debug, Clone)]
struct FileEntry {
  file: AlbumFile,
  selected: bool,
  status: FileStatus,
  progress: f32,
  downloaded: u64,
  speed: f64,
}

#[derive(Debug, Clone, PartialEq)]
enum AppStatus {
  Idle,
  Fetching,
  Downloading,
  Paused,
}

#[derive(Debug)]
enum UiUpdate {
  AlbumFetched(Vec<AlbumFile>),
  FetchError(String),
  FileStatus(usize, FileStatus),
  FileProgress(usize, f32, u64, f64),
}

#[derive(Debug)]
enum Command {
  Fetch(String),
  StartDownload(Vec<(usize, AlbumFile)>, PathBuf),
  PauseAll,
  ResumeAll,
  StopAll,
}

struct App {
  url: String,
  download_dir: String,
  files: Vec<FileEntry>,
  status: AppStatus,
  status_msg: String,
  state: Arc<Mutex<SharedState>>,
  cmd_tx: std::sync::mpsc::Sender<Command>,
  ui_rx: std::sync::mpsc::Receiver<UiUpdate>,
  rt: tokio::runtime::Handle,
  ctx: Option<egui::Context>,
}

struct SharedState {
  paused: bool,
  stopped: bool,
}

fn format_size(bytes: u64) -> String {
  const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
  let mut size = bytes as f64;
  for unit in UNITS {
    if size < 1024.0 || *unit == "TB" {
      return format!("{:.2} {}", size, unit);
    }
    size /= 1024.0;
  }
  format!("{:.2} TB", size)
}

fn normalize_album_json(raw: &str) -> String {
  let re_keys = Regex::new(r"(?m)^(\s*)([A-Za-z0-9_]+):").unwrap();
  let out = re_keys.replace_all(raw, r#"$1"$2":"#).to_string();
  let re_trailing = Regex::new(r",\s*([}\]])").unwrap();
  let out = re_trailing.replace_all(&out, "$1").to_string();
  out.replace("\\'", "'")
}

fn xor_decrypt(encrypted_b64: &str, timestamp: u64) -> Result<String, String> {
  let key = format!("SECRET_KEY_{}", timestamp / 3600);
  let key_bytes = key.as_bytes();
  let data = base64::engine::general_purpose::STANDARD
    .decode(encrypted_b64)
    .map_err(|e| format!("base64 error: {}", e))?;
  let decrypted: Vec<u8> = data
    .iter()
    .enumerate()
    .map(|(i, b)| b ^ key_bytes[i % key_bytes.len()])
    .collect();
  String::from_utf8(decrypted).map_err(|e| format!("utf8 error: {}", e))
}

async fn fetch_album(client: &reqwest::Client, url: &str) -> Result<Vec<AlbumFile>, String> {
  let advanced_url = if url.contains('?') {
    format!("{}&advanced=1", url)
  } else {
    format!("{}?advanced=1", url)
  };
  let resp = client
    .get(&advanced_url)
    .header(USER_AGENT, UA)
    .header(REFERER, "https://bunkr.pk/")
    .send()
    .await
    .map_err(|e| format!("Network error: {}", e))?;
  let html = resp
    .text()
    .await
    .map_err(|e| format!("Read error: {}", e))?;
  let re = Regex::new(r"window\.albumFiles\s*=\s*(\[[\s\S]*?\]);\s*\n").unwrap();
  let caps = re
    .captures(&html)
    .ok_or("Could not find album files in page. Check the URL.")?;
  let normalized = normalize_album_json(&caps[1]);
  serde_json::from_str(&normalized).map_err(|e| format!("Parse error: {}", e))
}

async fn resolve_download_url(client: &reqwest::Client, file_id: u64) -> Result<String, String> {
  let mut headers = HeaderMap::new();
  headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
  headers.insert(USER_AGENT, HeaderValue::from_static(UA));
  headers.insert(ORIGIN, HeaderValue::from_static("https://get.bunkrr.su"));
  headers.insert(
    REFERER,
    HeaderValue::from_str(&format!("https://get.bunkrr.su/file/{}", file_id))
      .map_err(|e| e.to_string())?,
  );
  let body = serde_json::json!({"id": file_id.to_string()});
  for api_url in API_URLS {
    for attempt in 0..3u32 {
      match client
        .post(*api_url)
        .headers(headers.clone())
        .json(&body)
        .send()
        .await
      {
        Ok(r) if r.status() == 429 => {
          tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempt) * 2)).await;
        }
        Ok(r) if r.status().is_success() => {
          let data: ApiResponse =
            r.json().await.map_err(|e| format!("API parse: {}", e))?;
          if !data.encrypted {
            return Err("API response not encrypted".to_string());
          }
          return xor_decrypt(&data.url, data.timestamp);
        }
        Ok(r) => {
          if attempt == 2 {
            return Err(format!("API HTTP {}", r.status()));
          }
        }
        Err(e) => {
          if attempt == 2 {
            return Err(format!("API error: {}", e));
          }
          tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
      }
    }
  }
  Err("All API endpoints failed".to_string())
}

fn sanitize_filename(name: &str) -> String {
  name
    .chars()
    .map(|c| match c {
      '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
      _ => c,
    })
    .collect()
}

fn spawn_background(
  rt: &tokio::runtime::Handle,
  cmd_rx: std::sync::mpsc::Receiver<Command>,
  ui_tx: std::sync::mpsc::Sender<UiUpdate>,
  state: Arc<Mutex<SharedState>>,
  ctx: egui::Context,
) {
  rt.spawn(async move {
    let client = reqwest::Client::builder()
      .redirect(reqwest::redirect::Policy::limited(10))
      .timeout(std::time::Duration::from_secs(600))
      .build()
      .unwrap();

    loop {
      let cmd = match cmd_rx.recv() {
        Ok(c) => c,
        Err(_) => break,
      };
      match cmd {
        Command::Fetch(url) => {
          match fetch_album(&client, &url).await {
            Ok(files) => {
              let _ = ui_tx.send(UiUpdate::AlbumFetched(files));
            }
            Err(e) => {
              let _ = ui_tx.send(UiUpdate::FetchError(e));
            }
          }
          ctx.request_repaint();
        }
        Command::StartDownload(items, dir) => {
          {
            let mut s = state.lock().unwrap();
            s.paused = false;
            s.stopped = false;
          }
          let sem = Arc::new(tokio::sync::Semaphore::new(CONCURRENT_DOWNLOADS));
          let mut handles = Vec::new();
          for (idx, file) in items {
            let client = client.clone();
            let ui_tx = ui_tx.clone();
            let state = state.clone();
            let dir = dir.clone();
            let sem = sem.clone();
            let ctx = ctx.clone();
            let handle = tokio::spawn(async move {
              let _permit = sem.acquire().await.unwrap();
              if state.lock().unwrap().stopped {
                let _ = ui_tx.send(UiUpdate::FileStatus(
                  idx,
                  FileStatus::Failed("Stopped".into()),
                ));
                ctx.request_repaint();
                return;
              }
              let _ = ui_tx.send(UiUpdate::FileStatus(idx, FileStatus::Resolving));
              ctx.request_repaint();
              let cdn_url = match resolve_download_url(&client, file.id).await {
                Ok(u) => u,
                Err(e) => {
                  let _ = ui_tx.send(UiUpdate::FileStatus(idx, FileStatus::Failed(e)));
                  ctx.request_repaint();
                  return;
                }
              };
              let _ = ui_tx.send(UiUpdate::FileStatus(idx, FileStatus::Downloading));
              ctx.request_repaint();
              let filename = sanitize_filename(&file.original);
              let dest = dir.join(&filename);
              if dest.exists() {
                if let Ok(meta) = std::fs::metadata(&dest) {
                  if meta.len() == file.size && file.size > 0 {
                    let _ = ui_tx.send(UiUpdate::FileProgress(idx, 1.0, file.size, 0.0));
                    let _ = ui_tx.send(UiUpdate::FileStatus(idx, FileStatus::Done));
                    ctx.request_repaint();
                    return;
                  }
                }
              }
              let resp = match client
                .get(&cdn_url)
                .header(USER_AGENT, UA)
                .header(REFERER, "https://get.bunkrr.su/")
                .send()
                .await
              {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                  let _ = ui_tx.send(UiUpdate::FileStatus(
                    idx,
                    FileStatus::Failed(format!("HTTP {}", r.status())),
                  ));
                  ctx.request_repaint();
                  return;
                }
                Err(e) => {
                  let _ = ui_tx.send(UiUpdate::FileStatus(
                    idx,
                    FileStatus::Failed(format!("{}", e)),
                  ));
                  ctx.request_repaint();
                  return;
                }
              };
              let total = resp.content_length().unwrap_or(file.size);
              let mut downloaded: u64 = 0;
              let start = Instant::now();
              let mut f = match tokio::fs::File::create(&dest).await {
                Ok(f) => f,
                Err(e) => {
                  let _ = ui_tx.send(UiUpdate::FileStatus(
                    idx,
                    FileStatus::Failed(format!("File create: {}", e)),
                  ));
                  ctx.request_repaint();
                  return;
                }
              };
              let mut stream = resp.bytes_stream();
              while let Some(chunk) = stream.next().await {
                if state.lock().unwrap().stopped {
                  let _ = ui_tx.send(UiUpdate::FileStatus(
                    idx,
                    FileStatus::Failed("Stopped".into()),
                  ));
                  ctx.request_repaint();
                  return;
                }
                while state.lock().unwrap().paused {
                  let _ = ui_tx.send(UiUpdate::FileStatus(idx, FileStatus::Paused));
                  ctx.request_repaint();
                  tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                  if state.lock().unwrap().stopped {
                    let _ = ui_tx.send(UiUpdate::FileStatus(
                      idx,
                      FileStatus::Failed("Stopped".into()),
                    ));
                    ctx.request_repaint();
                    return;
                  }
                }
                let _ = ui_tx.send(UiUpdate::FileStatus(idx, FileStatus::Downloading));
                match chunk {
                  Ok(bytes) => {
                    if f.write_all(&bytes).await.is_err() {
                      let _ = ui_tx.send(UiUpdate::FileStatus(
                        idx,
                        FileStatus::Failed("Write error".into()),
                      ));
                      ctx.request_repaint();
                      return;
                    }
                    downloaded += bytes.len() as u64;
                    let elapsed = start.elapsed().as_secs_f64();
                    let speed = if elapsed > 0.0 {
                      downloaded as f64 / elapsed
                    } else {
                      0.0
                    };
                    let progress = if total > 0 {
                      downloaded as f32 / total as f32
                    } else {
                      0.0
                    };
                    let _ =
                      ui_tx.send(UiUpdate::FileProgress(idx, progress, downloaded, speed));
                    ctx.request_repaint();
                  }
                  Err(e) => {
                    let _ = ui_tx.send(UiUpdate::FileStatus(
                      idx,
                      FileStatus::Failed(format!("Stream: {}", e)),
                    ));
                    ctx.request_repaint();
                    return;
                  }
                }
              }
              let _ = ui_tx.send(UiUpdate::FileProgress(idx, 1.0, downloaded, 0.0));
              let _ = ui_tx.send(UiUpdate::FileStatus(idx, FileStatus::Done));
              ctx.request_repaint();
            });
            handles.push(handle);
          }
          for h in handles {
            let _ = h.await;
          }
          ctx.request_repaint();
        }
        Command::PauseAll => {
          state.lock().unwrap().paused = true;
        }
        Command::ResumeAll => {
          state.lock().unwrap().paused = false;
        }
        Command::StopAll => {
          let mut s = state.lock().unwrap();
          s.stopped = true;
          s.paused = false;
        }
      }
    }
  });
}

impl App {
  fn new(rt: tokio::runtime::Handle) -> Self {
    let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel();
    let (_ui_tx, ui_rx) = std::sync::mpsc::channel();
    let state = Arc::new(Mutex::new(SharedState {
      paused: false,
      stopped: false,
    }));
    Self {
      url: String::new(),
      download_dir: dirs_default(),
      files: Vec::new(),
      status: AppStatus::Idle,
      status_msg: String::new(),
      state: state.clone(),
      cmd_tx,
      ui_rx,
      rt,
      ctx: None,
    }
  }

  fn process_updates(&mut self) {
    while let Ok(update) = self.ui_rx.try_recv() {
      match update {
        UiUpdate::AlbumFetched(album_files) => {
          self.files = album_files
            .into_iter()
            .map(|f| FileEntry {
              file: f,
              selected: true,
              status: FileStatus::Idle,
              progress: 0.0,
              downloaded: 0,
              speed: 0.0,
            })
            .collect();
          self.status = AppStatus::Idle;
          self.status_msg = format!("{} files loaded", self.files.len());
        }
        UiUpdate::FetchError(e) => {
          self.status = AppStatus::Idle;
          self.status_msg = format!("Error: {}", e);
        }
        UiUpdate::FileStatus(idx, st) => {
          if let Some(f) = self.files.get_mut(idx) {
            f.status = st;
          }
          let all_done = self
            .files
            .iter()
            .filter(|f| f.selected)
            .all(|f| matches!(f.status, FileStatus::Done | FileStatus::Failed(_)));
          if all_done
            && matches!(
              self.status,
              AppStatus::Downloading | AppStatus::Paused
            )
          {
            self.status = AppStatus::Idle;
            let done_count = self
              .files
              .iter()
              .filter(|f| f.status == FileStatus::Done)
              .count();
            let fail_count = self
              .files
              .iter()
              .filter(|f| matches!(f.status, FileStatus::Failed(_)))
              .count();
            self.status_msg = format!("Complete: {} done, {} failed", done_count, fail_count);
          }
        }
        UiUpdate::FileProgress(idx, progress, downloaded, speed) => {
          if let Some(f) = self.files.get_mut(idx) {
            f.progress = progress;
            f.downloaded = downloaded;
            f.speed = speed;
          }
        }
      }
    }
  }
}

fn dirs_default() -> String {
  dirs_path()
    .to_string_lossy()
    .to_string()
}

fn dirs_path() -> PathBuf {
  let home = std::env::var("HOME")
    .or_else(|_| std::env::var("USERPROFILE"))
    .unwrap_or_else(|_| ".".into());
  PathBuf::from(home).join("Downloads").join("bunkr")
}

impl eframe::App for App {
  fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
    if self.ctx.is_none() {
      self.ctx = Some(ctx.clone());
      let (cmd_tx2, cmd_rx) = std::sync::mpsc::channel::<Command>();
      let (ui_tx, ui_rx2) = std::sync::mpsc::channel::<UiUpdate>();
      let old_cmd_tx = std::mem::replace(&mut self.cmd_tx, cmd_tx2);
      drop(old_cmd_tx);
      let old_ui_rx = std::mem::replace(&mut self.ui_rx, ui_rx2);
      drop(old_ui_rx);
      spawn_background(&self.rt, cmd_rx, ui_tx, self.state.clone(), ctx.clone());
    }

    self.process_updates();

    let is_busy = matches!(self.status, AppStatus::Fetching | AppStatus::Downloading);

    egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
      ui.add_space(6.0);
      ui.horizontal(|ui| {
        ui.label(egui::RichText::new("🔗").size(16.0));
        ui.label("URL:");
        let url_edit = egui::TextEdit::singleline(&mut self.url)
          .desired_width(400.0)
          .hint_text("https://bunkr.pk/a/...");
        ui.add_enabled(!is_busy, url_edit);
        let fetch_btn = ui.add_enabled(
          !is_busy && !self.url.is_empty(),
          egui::Button::new("⟳ Fetch"),
        );
        if fetch_btn.clicked() {
          self.status = AppStatus::Fetching;
          self.status_msg = "Fetching album...".into();
          self.files.clear();
          let _ = self.cmd_tx.send(Command::Fetch(self.url.clone()));
        }
      });
      ui.add_space(2.0);
      ui.horizontal(|ui| {
        ui.label(egui::RichText::new("📁").size(16.0));
        ui.label("Folder:");
        ui.add_enabled(
          !is_busy,
          egui::TextEdit::singleline(&mut self.download_dir).desired_width(350.0),
        );
        if ui
          .add_enabled(!is_busy, egui::Button::new("Browse..."))
          .clicked()
        {
          if let Some(path) = rfd::FileDialog::new()
            .set_directory(&self.download_dir)
            .pick_folder()
          {
            self.download_dir = path.to_string_lossy().to_string();
          }
        }
      });
      ui.add_space(4.0);
    });

    egui::TopBottomPanel::bottom("controls").show(ctx, |ui| {
      ui.add_space(4.0);

      if !self.files.is_empty() {
        let selected_count = self.files.iter().filter(|f| f.selected).count();
        let selected_size: u64 = self
          .files
          .iter()
          .filter(|f| f.selected)
          .map(|f| f.file.size)
          .sum();
        let done_count = self
          .files
          .iter()
          .filter(|f| f.status == FileStatus::Done)
          .count();
        let total_downloaded: u64 = self
          .files
          .iter()
          .filter(|f| f.selected)
          .map(|f| f.downloaded)
          .sum();
        let total_speed: f64 = self
          .files
          .iter()
          .filter(|f| matches!(f.status, FileStatus::Downloading))
          .map(|f| f.speed)
          .sum();
        let overall_progress = if selected_size > 0 {
          total_downloaded as f32 / selected_size as f32
        } else {
          0.0
        };

        ui.horizontal(|ui| {
          ui.label(format!(
            "{}/{} selected  •  {}  •  {}/{}  •  ↓ {}/s",
            selected_count,
            self.files.len(),
            format_size(selected_size),
            done_count,
            selected_count,
            format_size(total_speed as u64),
          ));
        });
        let bar = egui::ProgressBar::new(overall_progress)
          .text(format!("{:.1}%", overall_progress * 100.0))
          .animate(matches!(self.status, AppStatus::Downloading));
        ui.add(bar);
      }

      ui.add_space(2.0);
      ui.horizontal(|ui| {
        let can_start = !self.files.is_empty()
          && matches!(self.status, AppStatus::Idle)
          && self.files.iter().any(|f| f.selected);
        if ui
          .add_enabled(can_start, egui::Button::new("▶ Start"))
          .clicked()
        {
          self.status = AppStatus::Downloading;
          self.status_msg = "Downloading...".into();
          let items: Vec<(usize, AlbumFile)> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.selected && !matches!(f.status, FileStatus::Done))
            .map(|(i, f)| (i, f.file.clone()))
            .collect();
          for (idx, _) in &items {
            if let Some(f) = self.files.get_mut(*idx) {
              f.status = FileStatus::Idle;
              f.progress = 0.0;
              f.downloaded = 0;
              f.speed = 0.0;
            }
          }
          let dir = PathBuf::from(&self.download_dir);
          let _ = self.cmd_tx.send(Command::StartDownload(items, dir));
        }

        let is_downloading = matches!(self.status, AppStatus::Downloading);
        let is_paused = matches!(self.status, AppStatus::Paused);

        if is_downloading {
          if ui.button("⏸ Pause").clicked() {
            self.status = AppStatus::Paused;
            self.status_msg = "Paused".into();
            let _ = self.cmd_tx.send(Command::PauseAll);
          }
        }
        if is_paused {
          if ui.button("▶ Resume").clicked() {
            self.status = AppStatus::Downloading;
            self.status_msg = "Downloading...".into();
            let _ = self.cmd_tx.send(Command::ResumeAll);
          }
        }
        if is_downloading || is_paused {
          if ui.button("⏹ Stop").clicked() {
            self.status = AppStatus::Idle;
            self.status_msg = "Stopped".into();
            let _ = self.cmd_tx.send(Command::StopAll);
          }
        }

        let has_failed = self
          .files
          .iter()
          .any(|f| matches!(f.status, FileStatus::Failed(_)));
        if has_failed && matches!(self.status, AppStatus::Idle) {
          if ui.button("↻ Retry Failed").clicked() {
            self.status = AppStatus::Downloading;
            self.status_msg = "Retrying failed...".into();
            let items: Vec<(usize, AlbumFile)> = self
              .files
              .iter()
              .enumerate()
              .filter(|(_, f)| matches!(f.status, FileStatus::Failed(_)))
              .map(|(i, f)| (i, f.file.clone()))
              .collect();
            for (idx, _) in &items {
              if let Some(f) = self.files.get_mut(*idx) {
                f.status = FileStatus::Idle;
                f.progress = 0.0;
                f.downloaded = 0;
                f.speed = 0.0;
              }
            }
            let dir = PathBuf::from(&self.download_dir);
            let _ = self.cmd_tx.send(Command::StartDownload(items, dir));
          }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
          if !self.status_msg.is_empty() {
            let color = if self.status_msg.starts_with("Error") {
              egui::Color32::from_rgb(255, 100, 100)
            } else {
              egui::Color32::from_rgb(150, 150, 150)
            };
            ui.colored_label(color, &self.status_msg);
          }
        });
      });
      ui.add_space(4.0);
    });

    egui::CentralPanel::default().show(ctx, |ui| {
      if self.files.is_empty() {
        if matches!(self.status, AppStatus::Fetching) {
          ui.centered_and_justified(|ui| {
            ui.spinner();
          });
        } else {
          ui.centered_and_justified(|ui| {
            ui.label(
              egui::RichText::new("Enter a Bunkr album URL and click Fetch")
                .size(16.0)
                .color(egui::Color32::from_rgb(120, 120, 120)),
            );
          });
        }
        return;
      }

      ui.horizontal(|ui| {
        if ui
          .add_enabled(!is_busy, egui::Button::new("☑ Select All"))
          .clicked()
        {
          for f in &mut self.files {
            f.selected = true;
          }
        }
        if ui
          .add_enabled(!is_busy, egui::Button::new("☐ Deselect All"))
          .clicked()
        {
          for f in &mut self.files {
            f.selected = false;
          }
        }
        let total_size: u64 = self.files.iter().map(|f| f.file.size).sum();
        ui.label(format!(
          "  {} files  •  {}",
          self.files.len(),
          format_size(total_size)
        ));
      });
      ui.separator();

      let row_height = 28.0;
      let num_rows = self.files.len();

      let snapshots: Vec<(String, u64, String, String, bool, FileStatus, f32, f64)> = self
        .files
        .iter()
        .map(|e| {
          (
            e.file.original.clone(),
            e.file.size,
            e.file.extension.clone(),
            e.file.timestamp.clone(),
            e.selected,
            e.status.clone(),
            e.progress,
            e.speed,
          )
        })
        .collect();

      egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_height, num_rows, |ui, row_range| {
          for idx in row_range {
            let (ref name, size, ref ext, ref ts, selected, ref status, progress, speed) =
              snapshots[idx];

            ui.horizontal(|ui| {
              let mut sel = selected;
              if ui.add_enabled(!is_busy, egui::Checkbox::new(&mut sel, "")).clicked() {
                self.files[idx].selected = sel;
              }

              ui.add(egui::Label::new(egui::RichText::new(name).size(13.0)).truncate());

              ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let status_text = match status {
                  FileStatus::Idle => egui::RichText::new("—")
                    .color(egui::Color32::from_rgb(120, 120, 120)),
                  FileStatus::Resolving => egui::RichText::new("resolving...")
                    .color(egui::Color32::from_rgb(200, 200, 100)),
                  FileStatus::Downloading => egui::RichText::new(format!(
                    "{:.0}%  {}/s",
                    progress * 100.0,
                    format_size(speed as u64)
                  ))
                  .color(egui::Color32::from_rgb(100, 180, 255)),
                  FileStatus::Paused => egui::RichText::new("paused")
                    .color(egui::Color32::from_rgb(255, 200, 50)),
                  FileStatus::Done => {
                    egui::RichText::new("✓").color(egui::Color32::from_rgb(100, 220, 100))
                  }
                  FileStatus::Failed(e) => {
                    let short = if e.len() > 30 { &e[..30] } else { e };
                    egui::RichText::new(format!("✗ {}", short))
                      .color(egui::Color32::from_rgb(255, 100, 100))
                  }
                };
                ui.label(status_text);

                if !ts.is_empty() {
                  ui.label(
                    egui::RichText::new(ts)
                      .size(11.0)
                      .color(egui::Color32::from_rgb(130, 130, 130)),
                  );
                }

                ui.label(
                  egui::RichText::new(ext)
                    .size(11.0)
                    .color(egui::Color32::from_rgb(167, 139, 250)),
                );

                ui.label(
                  egui::RichText::new(format_size(size))
                    .size(12.0)
                    .color(egui::Color32::from_rgb(180, 180, 180)),
                );

                if matches!(status, FileStatus::Downloading | FileStatus::Paused) {
                  ui.add(egui::ProgressBar::new(progress).desired_width(80.0));
                }
              });
            });
          }
        });
    });
  }
}

fn main() {
  let rt = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()
    .unwrap();

  let handle = rt.handle().clone();

  let options = eframe::NativeOptions {
    viewport: egui::ViewportBuilder::default()
      .with_inner_size([900.0, 600.0])
      .with_min_inner_size([600.0, 400.0])
      .with_title("Bunkr Downloader"),
    ..Default::default()
  };

  eframe::run_native(
    "Bunkr Downloader",
    options,
    Box::new(move |_cc| Ok(Box::new(App::new(handle)))),
  )
  .unwrap();
}

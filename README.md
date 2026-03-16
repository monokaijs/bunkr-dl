# Bunkr Downloader

A fast, native desktop app to download albums from Bunkr. Works on Windows, macOS, and Linux.

## Features

- **Album Fetching** — Paste a Bunkr album URL and instantly load the full file list
- **File Browser** — See every file with its name, size, type, and upload date
- **Selective Downloads** — Check/uncheck individual files, or use Select All / Deselect All
- **Download Folder Picker** — Choose where to save files with a native folder dialog
- **Download Controls** — Start, Pause, Resume, and Stop downloads at any time
- **Retry Failed** — One-click retry for any files that failed
- **Live Progress** — Per-file progress bars with download speed, plus an overall progress indicator
- **Smart Skip** — Already-downloaded files are automatically skipped
- **Concurrent Downloads** — Multiple files download in parallel for maximum speed
- **Refetch** — Reload the album at any time to check for new files
- **Cross-platform** — Native window on Windows, macOS (Intel + Apple Silicon), and Linux

## Installation

Download the latest release for your platform from the [Releases](https://github.com/monokaijs/bunkr-dl/releases) page.

| Platform | File |
|----------|------|
| Windows | `bunkr-dl-windows-x86_64.exe` |
| macOS (Intel) | `bunkr-dl-macos-x86_64` |
| macOS (Apple Silicon) | `bunkr-dl-macos-aarch64` |
| Linux | `bunkr-dl-linux-x86_64` |

### Linux

Make the binary executable after downloading:

```
chmod +x bunkr-dl-linux-x86_64
./bunkr-dl-linux-x86_64
```

## Usage

1. Launch the app
2. Paste a Bunkr album URL (e.g. `https://bunkr.pk/a/...`)
3. Click **Fetch** to load the file list
4. Select the files you want
5. Choose a download folder
6. Click **Start**

## License

MIT

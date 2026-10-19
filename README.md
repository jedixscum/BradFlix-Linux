# BradFlix for Linux

BradFlix is an independent Linux desktop app for discovering and playing titles from catalogs and compatible add-ons. It has a native window and plays supported streams inside the app, with mpv available as a fallback. It is not affiliated with, or a replacement for, paid streaming services; catalog and video availability depend on the sources you configure and your access rights.

## Features

- Profiles, optional PINs, watchlists, history, and resume progress
- A cinematic home screen, custom catalog sections, catalog ordering, and multiple themes
- Multi-genre discovery, search, anime and movies/shows sections, and a TV release calendar
- Stremio-compatible metadata, catalog, stream, and subtitle add-ons
- In-app video playback with optional mpv fallback, stream ranking, custom stream match rules, audio/subtitle preferences, and episode autoplay
- Quick search with Ctrl+K and back navigation with Alt+Left
- Local backup and optional file-based sync
- Linux x86_64 GitHub Actions builds and in-app update checks

BradFlix does not unlock DRM-protected subscription video. Use official services and accounts for their licensed streams. Add-ons are separate services; only add sources you trust and that are legal where you live.

## Build and run locally

Install Rust, curl, sha256sum, and the GTK/WebKit development libraries required by Wry. Install mpv if you want the external-player fallback. On Ubuntu 24.04, the native libraries are:

    sudo apt-get install build-essential pkg-config libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev mpv curl

Then build and run from this folder:

    cargo build --release
    ./target/release/bradflix

The app saves profiles and watch data in `~/.config/bradflix/state.json`.

## Build and publish a Linux update

Every push to `main` that changes Linux source starts **Actions → Linux build**. A successful run creates a `bradflix-linux-x86_64` artifact. Builds are not published automatically.

To publish a successful build as an in-app update:

1. Open **Actions → Linux build** and choose a successful run.
2. Copy the numeric run ID from its URL.
3. Open **Actions → Publish Linux update → Run workflow**.
4. Enter that run ID and start the workflow.

The publish workflow accepts only a completed successful Linux build from `main` and creates a public GitHub Release with the binary and its SHA-256 checksum. BradFlix checks for published Linux releases when a profile opens; **Settings → Updates** also checks on demand. It downloads only a newer Linux x86_64 release and verifies its checksum before replacing the app file. Restart BradFlix after installation.

The app must be able to write to its own install folder. The updater works directly for a user-owned install such as `~/.local/bin/bradflix`; a system-owned install needs administrator rights and should be updated through the system's installation method. Update checks use `curl` and checksum verification uses `sha256sum`.

## Data and privacy

There are no accounts, tracking, or analytics in BradFlix. Your profiles remain on your computer. The app contacts the GitHub Releases API for update checks, the metadata provider, add-ons you install, and video servers for sources you select.

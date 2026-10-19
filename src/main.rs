// BradFlix: a tiny Stremio add-on client.
//
// What this program does:
//   1. Starts a small web server that only your own computer can reach.
//   2. Opens the app in its own native window (a built-in web view).
//   3. Saves your add-ons and watch history to a file.
//   4. Plays videos by handing the link to mpv (fast, hardware-accelerated).

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    platform::unix::WindowExtUnix,
    window::WindowBuilder,
};
use tiny_http::{Header, Method, Request, Response, Server};
use wry::{WebViewBuilder, WebViewBuilderExtUnix};

const PAGE: &str = include_str!("index.html");
const EMPTY_STATE: &str = r#"{}"#;
const GITHUB_RELEASES: &str = "https://api.github.com/repos/jedixscum/BradFlix-Linux/releases?per_page=100";
const LINUX_BINARY_ASSET: &str = "bradflix-linux-x86_64";
const LINUX_CHECKSUM_ASSET: &str = "bradflix-linux-x86_64.sha256";

// The newest play request, and what mpv last told us about it.
static PLAY_COUNTER: AtomicU64 = AtomicU64::new(0);
static PLAYER: Mutex<String> = Mutex::new(String::new());

fn state_file() -> PathBuf {
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    config_dir.join("bradflix").join("state.json")
}

/// Where the first version of this app (called WuPlay Linux) kept its data.
fn legacy_state_file() -> PathBuf {
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    config_dir.join("wuplay-linux").join("state.json")
}

fn reply(body: &str, content_type: &str, status: u16) -> Response<std::io::Cursor<Vec<u8>>> {
    let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .expect("valid header");
    let no_cache = Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
        .expect("valid header");
    Response::from_string(body)
        .with_status_code(status)
        .with_header(header)
        .with_header(no_cache)
}

fn read_body(request: &mut Request) -> String {
    let mut text = String::new();
    // Never read more than 5 MB.
    let _ = request.as_reader().take(5_000_000).read_to_string(&mut text);
    text
}

fn header_value(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

/// Only our own page may talk to us. This stops random websites
/// from sending commands to the app running on your computer.
fn is_trusted(request: &Request, address: &str) -> bool {
    // Each BradFlix process gets its own loopback port. This avoids a newly
    // installed build accidentally displaying an older process's web page.
    let localhost = address.replacen("127.0.0.1:", "localhost:", 1);
    let allowed_hosts = [address, localhost.as_str()];
    let allowed_origins = [format!("http://{address}"), format!("http://{localhost}")];

    let host_ok = header_value(request, "Host")
        .map(|h| allowed_hosts.contains(&h.as_str()))
        .unwrap_or(false);
    let origin_ok = match header_value(request, "Origin") {
        Some(origin) => allowed_origins.iter().any(|allowed| allowed == &origin),
        None => true,
    };
    host_ok && origin_ok
}

fn is_web_link(text: &str) -> bool {
    text.starts_with("http://") || text.starts_with("https://")
}

fn github_json(url: &str) -> Result<Value, String> {
    let output = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location", "--max-time", "15", "--header", "Accept: application/vnd.github+json", "--header", "User-Agent: BradFlix-Linux"])
        .arg(url)
        .output()
        .map_err(|e| format!("Could not start curl: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("GitHub returned invalid release data: {e}"))
}

fn latest_linux_release() -> Result<Value, String> {
    let releases = github_json(GITHUB_RELEASES)?;
    let list = releases.as_array().ok_or("GitHub returned an unexpected releases list")?;
    list.iter()
        .filter(|release| release["draft"].as_bool() == Some(false) && release["prerelease"].as_bool() == Some(false))
        .filter(|release| release["tag_name"].as_str().map(|tag| tag.starts_with("linux-v")).unwrap_or(false))
        .max_by_key(|release| version_tuple(release["tag_name"].as_str().unwrap_or("").strip_prefix("linux-v").unwrap_or("")))
        .cloned()
        .ok_or_else(|| "No published Linux BradFlix update is available yet.".to_string())
}

fn release_asset<'a>(release: &'a Value, name: &str) -> Result<&'a str, String> {
    release["assets"].as_array()
        .and_then(|assets| assets.iter().find(|asset| asset["name"].as_str() == Some(name)))
        .and_then(|asset| asset["browser_download_url"].as_str())
        .ok_or_else(|| format!("The published Linux update is missing {name}."))
}

fn check_linux_update() -> String {
    let current = env!("CARGO_PKG_VERSION");
    match latest_linux_release() {
        Ok(release) => {
            let tag = release["tag_name"].as_str().unwrap_or("");
            let version = tag.strip_prefix("linux-v").unwrap_or("");
            let newer = version_tuple(version) > version_tuple(current);
            serde_json::json!({
                "ok": true, "current": current, "available": newer,
                "version": version, "tag": tag,
                "published_at": release["published_at"],
                "release_url": release["html_url"]
            }).to_string()
        }
        Err(error) => serde_json::json!({ "ok": false, "current": current, "error": error }).to_string(),
    }
}

fn version_tuple(version: &str) -> (u64, u64, u64) {
    let mut parts = version.split('.').map(|part| part.parse::<u64>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

fn download_asset(url: &str, destination: &PathBuf) -> Result<(), String> {
    let output = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location", "--max-time", "180", "--output"])
        .arg(destination)
        .arg(url)
        .output()
        .map_err(|e| format!("Could not start curl: {e}"))?;
    if output.status.success() { Ok(()) } else {
        let _ = std::fs::remove_file(destination);
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn install_latest_linux_update() -> Result<String, String> {
    let release = latest_linux_release()?;
    let tag = release["tag_name"].as_str().ok_or("The release has no version tag")?;
    let version = tag.strip_prefix("linux-v").ok_or("This is not a Linux release")?;
    if version_tuple(version) <= version_tuple(env!("CARGO_PKG_VERSION")) {
        return Err("BradFlix is already up to date.".to_string());
    }
    let binary_url = release_asset(&release, LINUX_BINARY_ASSET)?;
    let checksum_url = release_asset(&release, LINUX_CHECKSUM_ASSET)?;
    let executable = std::env::current_exe().map_err(|e| format!("Could not locate the running app: {e}"))?;
    let folder = executable.parent().ok_or("Could not locate the app folder")?;
    let temp_base = format!(".bradflix-update-{}", std::process::id());
    let temp_binary = folder.join(&temp_base);
    let temp_checksum = std::env::temp_dir().join(format!("{temp_base}.sha256"));
    let result = (|| {
        download_asset(binary_url, &temp_binary)?;
        download_asset(checksum_url, &temp_checksum)?;
        let expected = std::fs::read_to_string(&temp_checksum).map_err(|e| format!("Could not read update checksum: {e}"))?;
        let expected = expected.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("The update checksum file is invalid.".to_string());
        }
        let output = Command::new("sha256sum").arg(&temp_binary).output()
            .map_err(|e| format!("Could not verify update checksum (install sha256sum): {e}"))?;
        let actual = String::from_utf8_lossy(&output.stdout).split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        if !output.status.success() || actual != expected { return Err("The downloaded update failed its SHA-256 check.".to_string()); }
        let mut file = std::fs::File::open(&temp_binary).map_err(|e| format!("Could not open the downloaded app: {e}"))?;
        let mut magic = [0u8; 20];
        file.read_exact(&mut magic).map_err(|_| "The downloaded file is not a Linux executable.".to_string())?;
        if &magic[..4] != b"\x7fELF" || magic[18..20] != [0x3e, 0x00] {
            return Err("The update is not a compatible Linux x86_64 executable.".to_string());
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp_binary, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("Could not make the update executable: {e}"))?;
        std::fs::rename(&temp_binary, &executable).map_err(|e| format!("Could not replace {}: {e}", executable.display()))?;
        Ok(version.to_string())
    })();
    let _ = std::fs::remove_file(&temp_binary);
    let _ = std::fs::remove_file(&temp_checksum);
    result
}

/// Keeps only letters and commas, so language settings are always safe.
fn clean_langs(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphabetic() || *c == ',')
        .take(40)
        .collect()
}

fn set_player(id: u64, active: bool, key: &str, pos: f64, dur: f64) {
    // Only the newest playback is allowed to report.
    if PLAY_COUNTER.load(Ordering::SeqCst) != id {
        return;
    }
    let status = serde_json::json!({
        "id": id, "active": active, "key": key, "pos": pos, "dur": dur
    });
    if let Ok(mut guard) = PLAYER.lock() {
        *guard = status.to_string();
    }
}

/// Listens to mpv while it plays, so BradFlix knows how far you got.
fn watch_player(mut child: Child, socket: PathBuf, id: u64, key: String) {
    std::thread::spawn(move || {
        let mut pos = 0.0_f64;
        let mut dur = 0.0_f64;

        // mpv needs a moment before its control socket exists.
        let mut connection = None;
        for _ in 0..80 {
            if let Ok(stream) = UnixStream::connect(&socket) {
                connection = Some(stream);
                break;
            }
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }

        if let Some(mut stream) = connection {
            let _ = stream.write_all(b"{\"command\":[\"observe_property\",1,\"time-pos\"]}\n");
            let _ = stream.write_all(b"{\"command\":[\"observe_property\",2,\"duration\"]}\n");
            for line in BufReader::new(stream).lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(_) => break,
                };
                if let Ok(message) = serde_json::from_str::<Value>(&line) {
                    if message["event"] == "property-change" {
                        match message["name"].as_str() {
                            Some("time-pos") => {
                                if let Some(value) = message["data"].as_f64() {
                                    pos = value;
                                    set_player(id, true, &key, pos, dur);
                                }
                            }
                            Some("duration") => {
                                if let Some(value) = message["data"].as_f64() {
                                    dur = value;
                                    set_player(id, true, &key, pos, dur);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        let _ = child.wait();
        let _ = std::fs::remove_file(&socket);
        set_player(id, false, &key, pos, dur);
    });
}

fn play(body: &str) -> (String, u16) {
    let data: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let url = data["url"].as_str().unwrap_or("");
    let title = data["title"].as_str().unwrap_or("BradFlix");
    let key = data["key"].as_str().unwrap_or("").to_string();

    if !is_web_link(url) {
        return (r#"{"error":"That is not a web link."}"#.to_string(), 400);
    }

    let id = PLAY_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
    let socket =
        std::env::temp_dir().join(format!("bradflix-mpv-{}-{}.sock", std::process::id(), id));

    let mut command = Command::new("mpv");
    command
        .arg("--hwdec=auto-safe")
        .arg("--fs")
        .arg(format!("--input-ipc-server={}", socket.display()))
        .arg(format!("--force-media-title={}", title));

    if let Some(start) = data["start"].as_f64() {
        if start >= 5.0 {
            command.arg(format!("--start={}", start as u64));
        }
    }

    let audio = clean_langs(data["alang"].as_str().unwrap_or(""));
    if !audio.is_empty() {
        command.arg(format!("--alang={}", audio));
    }
    let subtitles = clean_langs(data["slang"].as_str().unwrap_or(""));
    if !subtitles.is_empty() {
        command.arg(format!("--slang={}", subtitles));
    }
    if data["subsOff"].as_bool().unwrap_or(false) {
        command.arg("--sid=no");
    }

    if let Some(list) = data["subs"].as_array() {
        for sub in list
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|v| is_web_link(v))
            .take(6)
        {
            command.arg(format!("--sub-file={}", sub));
        }
    }

    if let Some(list) = data["headers"].as_array() {
        for header in list
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|v| !v.contains('\n') && !v.contains('\r'))
            .take(20)
        {
            command.arg(format!("--http-header-fields-append={}", header));
        }
    }

    command.arg("--").arg(url);

    match command.spawn() {
        Ok(child) => {
            set_player(id, true, &key, 0.0, 0.0);
            watch_player(child, socket, id, key);
            (format!(r#"{{"ok":true,"id":{}}}"#, id), 200)
        }
        Err(_) => (
            r#"{"error":"Could not start mpv. Is it installed? Run: sudo pacman -S mpv"}"#
                .to_string(),
            500,
        ),
    }
}

/// Only .json files with a full path (or starting with ~/) may be read or written.
fn expand_path(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    let path = match raw.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
        None => PathBuf::from(raw),
    };
    if path.is_absolute() && path.extension().and_then(|e| e.to_str()) == Some("json") {
        Some(path)
    } else {
        None
    }
}

fn handle(mut request: Request, address: &str) {
    if !is_trusted(&request, address) {
        let _ = request.respond(reply("Forbidden", "text/plain", 403));
        return;
    }

    let path = request.url().split('?').next().unwrap_or("/").to_string();
    let method = request.method().clone();

    let response = match (method, path.as_str()) {
        (Method::Get, "/") => reply(PAGE, "text/html; charset=utf-8", 200),

        (Method::Get, "/api/state") => {
            let saved = std::fs::read_to_string(state_file())
                .or_else(|_| std::fs::read_to_string(legacy_state_file()))
                .unwrap_or_else(|_| EMPTY_STATE.to_string());
            reply(&saved, "application/json", 200)
        }

        (Method::Post, "/api/state") => {
            let body = read_body(&mut request);
            if serde_json::from_str::<Value>(&body).is_err() {
                reply(r#"{"error":"Bad data."}"#, "application/json", 400)
            } else {
                let file = state_file();
                if let Some(folder) = file.parent() {
                    let _ = std::fs::create_dir_all(folder);
                }
                match std::fs::write(&file, body) {
                    Ok(_) => reply(r#"{"ok":true}"#, "application/json", 200),
                    Err(_) => reply(r#"{"error":"Could not save."}"#, "application/json", 500),
                }
            }
        }

        (Method::Get, "/api/player") => {
            let status = PLAYER.lock().map(|g| g.clone()).unwrap_or_default();
            let status = if status.is_empty() {
                r#"{"id":0,"active":false}"#.to_string()
            } else {
                status
            };
            reply(&status, "application/json", 200)
        }

        (Method::Get, "/api/update/check") => reply(&check_linux_update(), "application/json", 200),

        (Method::Post, "/api/update/install") => match install_latest_linux_update() {
            Ok(version) => reply(&serde_json::json!({"ok": true, "version": version, "restartRequired": true}).to_string(), "application/json", 200),
            Err(error) => reply(&serde_json::json!({"ok": false, "error": error}).to_string(), "application/json", 500),
        },

        (Method::Post, "/api/file/read") => {
            let body = read_body(&mut request);
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let text = expand_path(data["path"].as_str().unwrap_or(""))
                .and_then(|path| std::fs::read_to_string(path).ok());
            match text {
                Some(text) => reply(
                    &serde_json::json!({ "content": text }).to_string(),
                    "application/json",
                    200,
                ),
                None => reply(
                    r#"{"error":"Could not read that file."}"#,
                    "application/json",
                    404,
                ),
            }
        }

        (Method::Post, "/api/file/write") => {
            let body = read_body(&mut request);
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let path = expand_path(data["path"].as_str().unwrap_or(""));
            let content = data["content"].as_str();
            match (path, content) {
                (Some(path), Some(content)) => {
                    if let Some(folder) = path.parent() {
                        let _ = std::fs::create_dir_all(folder);
                    }
                    match std::fs::write(&path, content) {
                        Ok(_) => reply(r#"{"ok":true}"#, "application/json", 200),
                        Err(_) => reply(
                            r#"{"error":"Could not write that file."}"#,
                            "application/json",
                            500,
                        ),
                    }
                }
                _ => reply(
                    r#"{"error":"The path must be a full path ending in .json"}"#,
                    "application/json",
                    400,
                ),
            }
        }

        (Method::Post, "/api/play") => {
            let body = read_body(&mut request);
            let (text, status) = play(&body);
            reply(&text, "application/json", status)
        }

        _ => reply("Not found", "text/plain", 404),
    };

    let _ = request.respond(response);
}

fn main() {
    // Some AMD/Wayland setups show a blank window without this.
    std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");

    // Bind to an available loopback port for this process. The old fixed-port
    // behavior silently reused another running build's web page when port
    // 7878 was already occupied, making installs look like they changed nothing.
    let server = Server::http("127.0.0.1:0")
        .expect("BradFlix could not start its local web server");
    let socket_addr = server
        .server_addr()
        .to_ip()
        .expect("BradFlix local server did not bind to an IP address");
    let address = format!("127.0.0.1:{}", socket_addr.port());
    let request_address = address.clone();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            handle(request, &request_address);
        }
    });

    // Make the window. Closing it quits the app.
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("BradFlix")
        .with_inner_size(LogicalSize::new(1360.0, 840.0))
        .with_min_inner_size(LogicalSize::new(640.0, 480.0))
        .build(&event_loop)
        .expect("could not create the window");

    // Put the web view inside the window.
    let vbox = window
        .default_vbox()
        .expect("could not find the window layout");
    let _webview = WebViewBuilder::new()
        .with_url(format!("http://{}", address))
        .build_gtk(vbox)
        .expect("could not create the web view (is webkit2gtk-4.1 installed?)");

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
        }
    });
}

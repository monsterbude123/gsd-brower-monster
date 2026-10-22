//! Integration coverage for capturing screenshots straight to a file.
//!
//! The file-backed path exists so a large screenshot never travels back to the
//! caller as base64 text: the daemon writes the bytes and reports the path.

use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn unique_temp_dir(test_name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "gsd-screenshot-{test_name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Browser-backed tests share one machine, so they take turns.
fn browser_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct BrowserTestEnv {
    home: PathBuf,
    artifacts: PathBuf,
    session: String,
}

impl BrowserTestEnv {
    fn new(name: &str) -> Self {
        let home = unique_temp_dir(name);
        let artifacts = home.join("artifacts");
        Self {
            session: format!("{name}-{}", std::process::id()),
            artifacts,
            home,
        }
    }

    fn output(&self, args: &[String]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_gsd-browser"))
            .env("GSD_BROWSER_ARTIFACT_DIR", &self.artifacts)
            .env("GSD_BROWSER_BROWSER_HEADLESS", "true")
            .env("GSD_BROWSER_DAEMON_STARTUP_TIMEOUT_SECONDS", "30")
            .args(["--session", &self.session])
            .args(args)
            .output()
            .expect("run gsd-browser")
    }

    fn json(&self, args: &[String]) -> Value {
        let mut full_args = vec!["--json".to_string()];
        full_args.extend(args.iter().cloned());
        let output = self.output(&full_args);
        assert!(
            output.status.success(),
            "command failed: args={full_args:?}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("parse JSON output")
    }

    fn stop(&self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_gsd-browser"))
            .env("GSD_BROWSER_ARTIFACT_DIR", &self.artifacts)
            .args(["--session", &self.session, "daemon", "stop"])
            .output();
    }
}

impl Drop for BrowserTestEnv {
    fn drop(&mut self) {
        self.stop();
        let _ = fs::remove_dir_all(&self.home);
    }
}

// ── Minimal local HTTP server ──

#[derive(Clone)]
struct ResponseSpec {
    content_type: &'static str,
    body: String,
}

struct TestServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn start(routes: HashMap<String, ResponseSpec>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let addr = listener.local_addr().expect("listener addr");
        let routes = Arc::new(routes);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => handle_connection(stream, &routes),
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(mut stream: TcpStream, routes: &HashMap<String, ResponseSpec>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }

    let raw_path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or("/");

    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) if header == "\r\n" => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let response = routes.get(path).cloned().unwrap_or(ResponseSpec {
        content_type: "text/plain; charset=utf-8",
        body: "not found".to_string(),
    });
    let bytes = response.body.as_bytes();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.content_type,
        bytes.len()
    );
    let _ = stream.write_all(bytes);
    let _ = stream.flush();
}

fn test_server() -> TestServer {
    let body = r#"<!doctype html>
<html>
  <head><meta charset="utf-8" /><title>Screenshot Output</title></head>
  <body>
    <h1 id="headline">Screenshot Output</h1>
    <button id="main-target">Main CTA</button>
  </body>
</html>"#
        .to_string();

    let mut routes = HashMap::new();
    routes.insert(
        "/".to_string(),
        ResponseSpec {
            content_type: "text/html; charset=utf-8",
            body,
        },
    );

    TestServer::start(routes)
}

// ── Helpers ──

fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.len() > 8 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n"
}

/// Read the pixel dimensions out of a PNG's IHDR chunk.
fn png_dimensions(bytes: &[u8]) -> (u64, u64) {
    assert!(is_png(bytes), "expected PNG magic bytes");
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (width as u64, height as u64)
}

/// Read the pixel dimensions out of a JPEG's first frame header.
fn jpeg_dimensions(bytes: &[u8]) -> (u64, u64) {
    assert!(is_jpeg(bytes), "expected JPEG magic bytes");
    let mut offset = 2;
    while offset + 9 < bytes.len() {
        if bytes[offset] != 0xFF {
            offset += 1;
            continue;
        }
        let marker = bytes[offset + 1];
        // SOF0/SOF1/SOF2 carry the frame dimensions.
        if matches!(marker, 0xC0 | 0xC1 | 0xC2) {
            let height = u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]) as u64;
            let width = u16::from_be_bytes([bytes[offset + 7], bytes[offset + 8]]) as u64;
            return (width, height);
        }
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
            offset += 2;
            continue;
        }
        let length = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        offset += 2 + length;
    }
    panic!("no JPEG frame header found");
}

fn open_page(env: &BrowserTestEnv, url: &str) {
    let _ = env.json(&["navigate".to_string(), url.to_string()]);
    let _ = env.json(&[
        "wait-for".to_string(),
        "--condition".to_string(),
        "text_visible".to_string(),
        "--value".to_string(),
        "Main CTA".to_string(),
        "--timeout".to_string(),
        "10000".to_string(),
    ]);
}

// ── Tests ──

#[test]
fn screenshot_output_writes_the_image_and_omits_the_base64_payload() {
    let _guard = browser_test_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let env = BrowserTestEnv::new("output");
    let server = test_server();
    open_page(&env, &server.base_url());

    let target = env.home.join("shot.jpg");
    let result = env.json(&[
        "screenshot".to_string(),
        "--output".to_string(),
        target.to_string_lossy().to_string(),
    ]);

    assert_eq!(result["written"], Value::Bool(true));
    assert_eq!(
        result["path"].as_str().unwrap_or_default(),
        target.to_string_lossy()
    );
    assert_eq!(result["mimeType"], "image/jpeg");
    assert!(
        result.get("data").is_none(),
        "file-backed capture must not carry a base64 payload: {result}"
    );

    let bytes = fs::read(&target).expect("screenshot file written");
    assert!(!bytes.is_empty(), "screenshot file is empty");
    assert!(is_jpeg(&bytes), "expected JPEG magic bytes");
    assert_eq!(
        result["byteLength"].as_u64().unwrap_or(0),
        bytes.len() as u64,
        "reported byte length should match the file"
    );
    let (width, height) = jpeg_dimensions(&bytes);
    assert_eq!(
        (result["width"].as_u64(), result["height"].as_u64()),
        (Some(width), Some(height)),
        "reported dimensions should match the captured image, not a DOM measurement"
    );
}

#[test]
fn screenshot_output_appends_the_format_extension_when_missing() {
    let _guard = browser_test_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let env = BrowserTestEnv::new("extension");
    let server = test_server();
    open_page(&env, &server.base_url());

    let target = env.home.join("no-extension");
    let result = env.json(&[
        "screenshot".to_string(),
        "--output".to_string(),
        target.to_string_lossy().to_string(),
    ]);

    let expected = env.home.join("no-extension.jpg");
    assert_eq!(
        result["path"].as_str().unwrap_or_default(),
        expected.to_string_lossy()
    );
    assert!(expected.exists(), "expected {}", expected.display());
}

#[test]
fn screenshot_element_crop_is_written_as_png() {
    let _guard = browser_test_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let env = BrowserTestEnv::new("element");
    let server = test_server();
    open_page(&env, &server.base_url());

    let target = env.home.join("element.png");
    let result = env.json(&[
        "screenshot".to_string(),
        "--selector".to_string(),
        "#main-target".to_string(),
        "--output".to_string(),
        target.to_string_lossy().to_string(),
    ]);

    assert_eq!(result["scope"], "element");
    assert_eq!(result["mimeType"], "image/png");
    assert!(
        result.get("data").is_none(),
        "file-backed capture must not carry a base64 payload: {result}"
    );

    let bytes = fs::read(&target).expect("element screenshot written");
    assert!(is_png(&bytes), "expected PNG magic bytes");
    let (width, height) = png_dimensions(&bytes);
    assert_eq!(
        (result["width"].as_u64(), result["height"].as_u64()),
        (Some(width), Some(height)),
        "reported dimensions should match the cropped image"
    );
}

#[test]
fn the_output_extension_decides_the_encoding() {
    let _guard = browser_test_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let env = BrowserTestEnv::new("ext-format");
    let server = test_server();
    open_page(&env, &server.base_url());

    // A `.png` path has to hold PNG bytes — a JPEG file wearing a `.png` name
    // is a broken image, whatever the format flag says.
    let target = env.home.join("named.png");
    let result = env.json(&[
        "screenshot".to_string(),
        "--output".to_string(),
        target.to_string_lossy().to_string(),
    ]);

    assert_eq!(result["mimeType"], "image/png");
    let bytes = fs::read(&target).expect("png screenshot written");
    assert!(
        is_png(&bytes),
        "expected PNG magic bytes in a file named .png"
    );

    // The same capture with a `.jpg` extension gets JPEG bytes.
    let target = env.home.join("named.jpg");
    let result = env.json(&[
        "screenshot".to_string(),
        "--output".to_string(),
        target.to_string_lossy().to_string(),
    ]);

    assert_eq!(result["mimeType"], "image/jpeg");
    let bytes = fs::read(&target).expect("jpeg screenshot written");
    assert!(
        is_jpeg(&bytes),
        "expected JPEG magic bytes in a file named .jpg"
    );
}

#[test]
fn screenshot_reports_the_dimensions_of_the_png_it_wrote() {
    let _guard = browser_test_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let env = BrowserTestEnv::new("png-format");
    let server = test_server();
    open_page(&env, &server.base_url());

    let target = env.home.join("shot.png");
    let result = env.json(&[
        "screenshot".to_string(),
        "--format".to_string(),
        "png".to_string(),
        "--output".to_string(),
        target.to_string_lossy().to_string(),
    ]);

    assert_eq!(result["mimeType"], "image/png");
    let bytes = fs::read(&target).expect("png screenshot written");
    assert!(is_png(&bytes), "expected PNG magic bytes");
    let (width, height) = png_dimensions(&bytes);
    assert_eq!(
        (result["width"].as_u64(), result["height"].as_u64()),
        (Some(width), Some(height)),
        "reported dimensions should match the PNG that was written"
    );
}

#[test]
fn screenshot_save_writes_into_the_artifacts_directory() {
    let _guard = browser_test_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let env = BrowserTestEnv::new("save");
    let server = test_server();
    open_page(&env, &server.base_url());

    let result = env.json(&["screenshot".to_string(), "--save".to_string()]);

    assert_eq!(result["written"], Value::Bool(true));
    let path = PathBuf::from(result["path"].as_str().expect("saved path"));
    assert!(
        path.starts_with(&env.artifacts),
        "expected a file under {}, got {}",
        env.artifacts.display(),
        path.display()
    );
    assert!(path.exists(), "expected {} to exist", path.display());
    assert!(
        is_jpeg(&fs::read(&path).expect("saved image")),
        "expected JPEG"
    );
}

#[test]
fn screenshot_without_a_destination_still_returns_base64() {
    let _guard = browser_test_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let env = BrowserTestEnv::new("base64");
    let server = test_server();
    open_page(&env, &server.base_url());

    let result = env.json(&["screenshot".to_string()]);

    assert_eq!(result["written"], Value::Bool(false));
    assert!(
        result.get("path").is_none(),
        "no destination was requested: {result}"
    );
    let data = result["data"].as_str().expect("base64 payload");
    assert!(!data.is_empty(), "expected a non-empty base64 payload");
}

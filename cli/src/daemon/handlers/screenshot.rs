use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::Page;
use image::ImageReader;
use serde_json::{json, Value};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use tracing::debug;

/// Handle screenshot capture — viewport JPEG (default), element crop, or full-page JPEG.
///
/// Params:
///   selector     (string, optional) — CSS selector for element crop
///   full_page    (bool, default false) — capture entire scrollable page
///   quality      (u32, default 80) — JPEG compression quality 1-100
///   format       (string, optional) — "jpeg" or "png"; decides the encoding
///                                       when the output path does not name one
///   output       (string, optional) — write the raw image bytes to this path
///   save         (bool, default false) — write to a generated name in the
///                                       artifacts directory
///   include_data (bool, optional) — force the base64 payload into the response
///
/// The output path's extension decides the encoding, so `--output shot.png`
/// really writes PNG bytes. When the capture is written to disk the response
/// carries `path` and omits the base64 payload, so a large screenshot never
/// crosses the IPC boundary as text.
pub async fn handle_screenshot(page: &Page, params: &Value) -> Result<Value, String> {
    let selector = params
        .get("selector")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let full_page = params
        .get("full_page")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let quality = params.get("quality").and_then(|v| v.as_u64()).unwrap_or(80) as i64;
    let requested_format = params
        .get("format")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let output = params
        .get("output")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let save = params
        .get("save")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // The file the caller will open decides the encoding. A `.png` path that
    // holds JPEG bytes is a broken image no matter what the format flag says.
    let format = resolve_format(requested_format, output);

    let capture = capture_image(page, selector, full_page, quality, format).await?;

    // The bytes on disk are the source of truth for the reported type.
    let mime_type = sniff_mime_type(&capture.bytes).unwrap_or(capture.mime_type);
    let extension = if mime_type == "image/png" {
        "png"
    } else {
        "jpg"
    };

    let destination = resolve_destination(output, save, extension, page).await?;

    // A file-backed capture omits the base64 payload unless the caller asks for
    // it explicitly — the whole point of the file option is that the image
    // stops travelling as text.
    let include_data = params
        .get("include_data")
        .and_then(|v| v.as_bool())
        .unwrap_or(destination.is_none());

    let mut response = json!({
        "mimeType": mime_type,
        "width": capture.width,
        "height": capture.height,
        "scope": capture.scope,
        "byteLength": capture.bytes.len(),
        "written": false,
    });
    if let Some(sel) = capture.selector.as_deref() {
        response["selector"] = json!(sel);
    }

    if let Some(path) = destination {
        write_capture(&path, &capture.bytes)?;
        response["path"] = json!(path.to_string_lossy());
        response["written"] = json!(true);
        debug!(
            "[screenshot] wrote {} bytes to {}",
            capture.bytes.len(),
            path.display()
        );
    }

    if include_data {
        response["data"] = json!(BASE64.encode(&capture.bytes));
    }

    Ok(response)
}

/// A captured image plus the metadata reported alongside it.
struct Capture {
    bytes: Vec<u8>,
    mime_type: &'static str,
    width: u64,
    height: u64,
    scope: &'static str,
    selector: Option<String>,
}

/// The image encodings a capture can be written in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    /// The MIME type the encoder produces.
    fn mime_type(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
        }
    }

    /// The file extension that goes with the encoding.
    fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
        }
    }

    /// The CDP format enum value.
    fn cdp_format(self) -> CaptureScreenshotFormat {
        match self {
            ImageFormat::Png => CaptureScreenshotFormat::Png,
            ImageFormat::Jpeg => CaptureScreenshotFormat::Jpeg,
        }
    }

    /// Parse a format name from a parameter or a file extension.
    fn parse(value: &str) -> Option<Self> {
        match value
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .as_str()
        {
            "png" => Some(ImageFormat::Png),
            "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
            _ => None,
        }
    }
}

/// Decide which encoding a capture should use.
///
/// The destination path wins when it names an image format: the caller asked
/// for a file called `shot.png`, so it gets PNG bytes. An explicit `format`
/// parameter is honoured when the path does not settle the question, and JPEG
/// is the fallback for everything else.
fn resolve_format(requested: Option<&str>, output: Option<&str>) -> ImageFormat {
    if let Some(path) = output {
        if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) {
            if let Some(format) = ImageFormat::parse(ext) {
                return format;
            }
        }
    }
    requested
        .and_then(ImageFormat::parse)
        .unwrap_or(ImageFormat::Jpeg)
}

/// Identify an image's encoding from its leading bytes.
fn sniff_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else {
        None
    }
}

/// Take the image bytes for the requested scope.
async fn capture_image(
    page: &Page,
    selector: Option<&str>,
    full_page: bool,
    quality: i64,
    format: ImageFormat,
) -> Result<Capture, String> {
    // Element screenshot — cropped to the element's bounding box
    if let Some(sel) = selector {
        debug!("[screenshot] element crop: {sel}");
        let element = page
            .find_element(sel)
            .await
            .map_err(|e| format!("element not found '{sel}': {e}"))?;

        let bytes = element
            .screenshot(format.cdp_format())
            .await
            .map_err(|e| format!("element screenshot failed: {e}"))?;

        // The captured pixels are the truth; the DOM box is only a fallback.
        let (width, height) = match image_dimensions(&bytes) {
            Some(dims) => dims,
            None => get_element_dimensions(page, sel).await,
        };

        return Ok(Capture {
            bytes,
            mime_type: format.mime_type(),
            width,
            height,
            scope: "element",
            selector: Some(sel.to_string()),
        });
    }

    // Viewport or full-page screenshot
    let scope = if full_page { "fullPage" } else { "viewport" };
    debug!(
        "[screenshot] {scope} capture, format={}, quality={quality}",
        format.extension()
    );

    let mut builder = ScreenshotParams::builder().format(format.cdp_format());

    // Quality only applies to JPEG
    if format == ImageFormat::Jpeg {
        builder = builder.quality(quality);
    }

    if full_page {
        builder = builder.full_page(true).capture_beyond_viewport(true);
    }

    let screenshot_params = builder.build();

    let bytes = page
        .screenshot(screenshot_params)
        .await
        .map_err(|e| format!("screenshot failed: {e}"))?;

    // The captured pixels are the truth. A DOM measurement reports the layout
    // viewport, which excludes the scrollbar and disagrees with what was
    // actually captured — a full-page shot is the window size, not the scroll
    // area minus the scrollbar.
    let (width, height) = match image_dimensions(&bytes) {
        Some(dims) => dims,
        None if full_page => get_full_page_dimensions(page).await,
        None => get_viewport_dimensions(page).await,
    };

    Ok(Capture {
        bytes,
        mime_type: format.mime_type(),
        width,
        height,
        scope,
        selector: None,
    })
}

/// Read an image's pixel dimensions straight from its header.
fn image_dimensions(bytes: &[u8]) -> Option<(u64, u64)> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let (width, height) = reader.into_dimensions().ok()?;
    Some((width as u64, height as u64))
}

/// Decide where the image should be written, if anywhere.
///
/// `output` names an explicit path. `save` without `output` generates a name in
/// the artifacts directory. A caller-supplied extension is left untouched so
/// `--output shot.png` writes exactly to that path; a missing one is filled in
/// with the format the capture actually used.
async fn resolve_destination(
    output: Option<&str>,
    save: bool,
    extension: &str,
    page: &Page,
) -> Result<Option<PathBuf>, String> {
    if let Some(path) = output {
        return Ok(Some(with_extension(PathBuf::from(path), extension)));
    }
    if !save {
        return Ok(None);
    }
    let title = page
        .get_title()
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "page".to_string());
    Ok(Some(
        screenshots_dir().join(auto_filename(&title, extension)),
    ))
}

/// Append the format extension when the caller did not name one.
fn with_extension(path: PathBuf, extension: &str) -> PathBuf {
    if path.extension().is_some() {
        return path;
    }
    let mut name = path.into_os_string();
    name.push(".");
    name.push(extension);
    PathBuf::from(name)
}

/// Build a timestamped filename from the page title.
fn auto_filename(title: &str, extension: &str) -> String {
    let sanitized: String = title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(50)
        .collect();
    // A title made entirely of punctuation leaves nothing worth reading.
    let sanitized = if sanitized.chars().any(|c| c.is_alphanumeric()) {
        sanitized
    } else {
        "screenshot".to_string()
    };
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    format!("{sanitized}-{ts}.{extension}")
}

/// The directory generated screenshot files land in.
fn screenshots_dir() -> PathBuf {
    artifacts_root().join("screenshots")
}

/// Root directory for artifacts (screenshots, PDFs, traces, debug bundles).
fn artifacts_root() -> PathBuf {
    std::env::var("GSD_BROWSER_ARTIFACT_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".gsd-browser")
                .join("artifacts")
        })
}

/// Write the captured bytes, creating parent directories as needed.
fn write_capture(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create output dir '{}': {e}", parent.display()))?;
        }
    }
    std::fs::write(path, bytes)
        .map_err(|e| format!("failed to write screenshot to '{}': {e}", path.display()))
}

/// Get viewport dimensions via JS.
async fn get_viewport_dimensions(page: &Page) -> (u64, u64) {
    let js = "JSON.stringify({w: window.innerWidth, h: window.innerHeight})";
    match page.evaluate_expression(js).await {
        Ok(val) => {
            if let Some(s) = val.value().and_then(|v| v.as_str().map(String::from)) {
                if let Ok(parsed) = serde_json::from_str::<Value>(&s) {
                    let w = parsed.get("w").and_then(|v| v.as_u64()).unwrap_or(0);
                    let h = parsed.get("h").and_then(|v| v.as_u64()).unwrap_or(0);
                    return (w, h);
                }
            }
            (0, 0)
        }
        Err(_) => (0, 0),
    }
}

/// Get full scrollable page dimensions via JS.
async fn get_full_page_dimensions(page: &Page) -> (u64, u64) {
    let js = "JSON.stringify({w: Math.max(document.documentElement.scrollWidth, document.documentElement.clientWidth), h: Math.max(document.documentElement.scrollHeight, document.documentElement.clientHeight)})";
    match page.evaluate_expression(js).await {
        Ok(val) => {
            if let Some(s) = val.value().and_then(|v| v.as_str().map(String::from)) {
                if let Ok(parsed) = serde_json::from_str::<Value>(&s) {
                    let w = parsed.get("w").and_then(|v| v.as_u64()).unwrap_or(0);
                    let h = parsed.get("h").and_then(|v| v.as_u64()).unwrap_or(0);
                    return (w, h);
                }
            }
            (0, 0)
        }
        Err(_) => (0, 0),
    }
}

/// Get element bounding box dimensions via JS.
async fn get_element_dimensions(page: &Page, selector: &str) -> (u64, u64) {
    let js = format!(
        r#"(() => {{
            const el = document.querySelector({sel});
            if (!el) return JSON.stringify({{w: 0, h: 0}});
            const r = el.getBoundingClientRect();
            return JSON.stringify({{w: Math.round(r.width), h: Math.round(r.height)}});
        }})()"#,
        sel = serde_json::to_string(selector).unwrap_or_else(|_| format!("\"{}\"", selector))
    );
    match page.evaluate_expression(&js).await {
        Ok(val) => {
            if let Some(s) = val.value().and_then(|v| v.as_str().map(String::from)) {
                if let Ok(parsed) = serde_json::from_str::<Value>(&s) {
                    let w = parsed.get("w").and_then(|v| v.as_u64()).unwrap_or(0);
                    let h = parsed.get("h").and_then(|v| v.as_u64()).unwrap_or(0);
                    return (w, h);
                }
            }
            (0, 0)
        }
        Err(_) => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_extension_appends_only_when_missing() {
        let with = with_extension(PathBuf::from("/tmp/shot"), "jpg");
        assert_eq!(with, PathBuf::from("/tmp/shot.jpg"));

        let explicit = with_extension(PathBuf::from("/tmp/shot.png"), "jpg");
        assert_eq!(explicit, PathBuf::from("/tmp/shot.png"));
    }

    #[test]
    fn auto_filename_sanitizes_the_title() {
        let name = auto_filename("Example Domain / Home", "jpg");
        assert!(name.starts_with("Example_Domain___Home-"));
        assert!(name.ends_with(".jpg"));
    }

    #[test]
    fn auto_filename_falls_back_when_title_is_unusable() {
        for title in ["///", "   ", ""] {
            let name = auto_filename(title, "png");
            assert!(
                name.starts_with("screenshot-"),
                "expected a readable fallback for {title:?}, got {name}"
            );
            assert!(name.ends_with(".png"));
        }
    }

    #[test]
    fn the_output_extension_decides_the_encoding() {
        // A `.png` path has to hold PNG bytes, whatever the format flag says.
        assert_eq!(
            resolve_format(Some("jpeg"), Some("/tmp/shot.png")),
            ImageFormat::Png
        );
        assert_eq!(
            resolve_format(None, Some("/tmp/shot.png")),
            ImageFormat::Png
        );
        assert_eq!(
            resolve_format(Some("png"), Some("/tmp/shot.jpg")),
            ImageFormat::Jpeg
        );
        assert_eq!(
            resolve_format(None, Some("/tmp/shot.jpeg")),
            ImageFormat::Jpeg
        );
    }

    #[test]
    fn the_format_parameter_applies_when_the_path_says_nothing() {
        assert_eq!(resolve_format(Some("png"), None), ImageFormat::Png);
        assert_eq!(resolve_format(Some("jpeg"), None), ImageFormat::Jpeg);
        // No format and no path — JPEG is the default.
        assert_eq!(resolve_format(None, None), ImageFormat::Jpeg);
        // A path without a recognised image extension falls back to the flag.
        assert_eq!(
            resolve_format(Some("png"), Some("/tmp/shot.bin")),
            ImageFormat::Png
        );
    }

    #[test]
    fn mime_type_is_read_from_the_bytes() {
        assert_eq!(
            sniff_mime_type(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0]),
            Some("image/png")
        );
        assert_eq!(
            sniff_mime_type(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_mime_type(b"not an image"), None);
    }
}

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::Page;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tracing::debug;

/// Handle screenshot capture — viewport JPEG (default), element PNG, or full-page JPEG.
///
/// Params:
///   selector     (string, optional) — CSS selector for element crop (forces PNG)
///   full_page    (bool, default false) — capture entire scrollable page
///   quality      (u32, default 80) — JPEG compression quality 1-100
///   format       (string, default "jpeg") — "jpeg" or "png"
///   output       (string, optional) — write the raw image bytes to this path
///   save         (bool, default false) — write to a generated name in the
///                                       artifacts directory
///   include_data (bool, optional) — force the base64 payload into the response
///
/// When the capture is written to disk the response carries `path` and omits the
/// base64 payload, so a large screenshot never crosses the IPC boundary as text.
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
    let format_str = params
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("jpeg");
    let output = params
        .get("output")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let save = params
        .get("save")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let capture = capture_image(page, selector, full_page, quality, format_str).await?;

    let extension = if capture.mime_type == "image/png" {
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
        "mimeType": capture.mime_type,
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

/// Take the image bytes for the requested scope.
async fn capture_image(
    page: &Page,
    selector: Option<&str>,
    full_page: bool,
    quality: i64,
    format_str: &str,
) -> Result<Capture, String> {
    // Element screenshot — always PNG, uses element bounding box clip
    if let Some(sel) = selector {
        debug!("[screenshot] element crop: {sel}");
        let element = page
            .find_element(sel)
            .await
            .map_err(|e| format!("element not found '{sel}': {e}"))?;

        let bytes = element
            .screenshot(CaptureScreenshotFormat::Png)
            .await
            .map_err(|e| format!("element screenshot failed: {e}"))?;

        // Get element bounding box for dimensions
        let dims = get_element_dimensions(page, sel).await;

        return Ok(Capture {
            bytes,
            mime_type: "image/png",
            width: dims.0,
            height: dims.1,
            scope: "element",
            selector: Some(sel.to_string()),
        });
    }

    // Viewport or full-page screenshot
    let (cdp_format, mime_type) = match format_str {
        "png" => (CaptureScreenshotFormat::Png, "image/png"),
        _ => (CaptureScreenshotFormat::Jpeg, "image/jpeg"),
    };

    let scope = if full_page { "fullPage" } else { "viewport" };
    debug!("[screenshot] {scope} capture, format={format_str}, quality={quality}");

    let mut builder = ScreenshotParams::builder().format(cdp_format);

    // Quality only applies to JPEG
    if format_str != "png" {
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

    // Get viewport dimensions for metadata
    let (width, height) = if full_page {
        get_full_page_dimensions(page).await
    } else {
        get_viewport_dimensions(page).await
    };

    Ok(Capture {
        bytes,
        mime_type,
        width,
        height,
        scope,
        selector: None,
    })
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
}

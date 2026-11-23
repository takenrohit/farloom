//! MIME type lookup by file extension (Requirement 7.3).

use std::path::Path;

/// Returns the `Content-Type` value for `path` based on its extension.
///
/// Matching is case-insensitive. Unknown or missing extensions give
/// `application/octet-stream`.
pub fn mime_for_path(path: &Path) -> &'static str {
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return "application/octet-stream",
    };
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &[(&str, &str)] = &[
        ("html", "text/html; charset=utf-8"),
        ("htm", "text/html; charset=utf-8"),
        ("css", "text/css; charset=utf-8"),
        ("js", "text/javascript; charset=utf-8"),
        ("json", "application/json"),
        ("txt", "text/plain; charset=utf-8"),
        ("png", "image/png"),
        ("jpg", "image/jpeg"),
        ("jpeg", "image/jpeg"),
        ("gif", "image/gif"),
        ("svg", "image/svg+xml"),
        ("ico", "image/x-icon"),
    ];

    #[test]
    fn every_table_entry_maps_correctly() {
        for (ext, mime) in TABLE {
            let name = format!("file.{ext}");
            assert_eq!(mime_for_path(Path::new(&name)), *mime, "{name}");
        }
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(mime_for_path(Path::new("INDEX.HTML")), "text/html; charset=utf-8");
        assert_eq!(mime_for_path(Path::new("photo.JpEg")), "image/jpeg");
        assert_eq!(mime_for_path(Path::new("a/b/Style.CSS")), "text/css; charset=utf-8");
    }

    #[test]
    fn unknown_or_missing_extension_is_octet_stream() {
        assert_eq!(mime_for_path(Path::new("archive.zip")), "application/octet-stream");
        assert_eq!(mime_for_path(Path::new("README")), "application/octet-stream");
        assert_eq!(mime_for_path(Path::new(".html")), "application/octet-stream");
        assert_eq!(mime_for_path(Path::new("dir/")), "application/octet-stream");
    }

    #[test]
    fn only_last_extension_counts() {
        assert_eq!(mime_for_path(Path::new("bundle.min.js")), "text/javascript; charset=utf-8");
        assert_eq!(mime_for_path(Path::new("page.html.bak")), "application/octet-stream");
    }
}

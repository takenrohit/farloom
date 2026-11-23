//! Static file handler with path-traversal protection.
//!
//! [`StaticFiles::resolve`] maps a raw (percent-encoded) request path to a
//! canonical file path under the static root. Cheap string checks reject
//! obvious attacks with a clear log reason; the canonical containment check
//! (`starts_with(&root)`) is the real guarantee.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::http::percent::percent_decode;
use crate::http::request::{Method, Request};
use crate::http::response::{Response, StatusCode};
use crate::log;
use crate::mime::mime_for_path;

/// Serves files from one canonical root directory.
#[derive(Debug, Clone)]
pub struct StaticFiles {
    root: PathBuf,
}

#[derive(Debug)]
pub enum ResolveError {
    /// A traversal rule fired; the string names the rule. Gives 404 plus a WARN log.
    Traversal(&'static str),
    /// No such file, or a directory without `index.html`. Gives 404.
    NotFound,
    /// Any other I/O error. Gives 500 plus an ERROR log.
    Io(io::Error),
}

/// True for errors that mean "this path does not name a file", including the
/// Windows-specific errors produced when a file is used as a directory
/// (`/index.html/x`) or the name is invalid.
fn is_not_found(e: &io::Error) -> bool {
    if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidInput) {
        return true;
    }
    #[cfg(windows)]
    {
        // 2 FILE_NOT_FOUND, 3 PATH_NOT_FOUND, 123 INVALID_NAME,
        // 161 BAD_PATHNAME, 267 DIRECTORY (not a directory).
        matches!(e.raw_os_error(), Some(2 | 3 | 123 | 161 | 267))
    }
    #[cfg(not(windows))]
    {
        // ENOTDIR, ENAMETOOLONG.
        matches!(e.raw_os_error(), Some(20 | 36))
    }
}

fn map_io(e: io::Error) -> ResolveError {
    if is_not_found(&e) {
        ResolveError::NotFound
    } else {
        ResolveError::Io(e)
    }
}

impl StaticFiles {
    /// `canonical_root` must already be canonical (see `Config::validate_static_root`).
    pub fn new(canonical_root: PathBuf) -> StaticFiles {
        StaticFiles {
            root: canonical_root,
        }
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Resolves a raw request path to a canonical regular file inside the root.
    pub fn resolve(&self, raw_path: &str) -> Result<PathBuf, ResolveError> {
        // 1. Decode.
        let bytes = percent_decode(raw_path).map_err(|_| ResolveError::Traversal("bad encoding"))?;
        // 2. UTF-8.
        let decoded = String::from_utf8(bytes).map_err(|_| ResolveError::NotFound)?;

        // 3. Whole-path checks.
        if decoded.contains('\0') {
            return Err(ResolveError::Traversal("nul"));
        }
        if decoded.contains('\\') {
            return Err(ResolveError::Traversal("backslash"));
        }
        if decoded.starts_with("//") {
            return Err(ResolveError::Traversal("leading //"));
        }

        // 4. Segment checks.
        let mut candidate = self.root.clone();
        for segment in decoded.split('/') {
            if segment.is_empty() || segment == "." {
                continue;
            }
            if segment == ".." {
                return Err(ResolveError::Traversal("dotdot"));
            }
            if segment.contains(':') {
                return Err(ResolveError::Traversal("colon"));
            }
            candidate.push(segment);
        }

        // 5. Directory index.
        if candidate.is_dir() {
            candidate.push("index.html");
        }

        // 6. Canonicalize.
        let canonical = fs::canonicalize(&candidate).map_err(map_io)?;

        // 7. Containment (component-wise).
        if !canonical.starts_with(&self.root) {
            return Err(ResolveError::Traversal("outside root"));
        }

        // 8. Regular file.
        if !canonical.is_file() {
            return Err(ResolveError::NotFound);
        }
        Ok(canonical)
    }

    /// Serves a request without a known client address.
    pub fn serve(&self, req: &Request) -> Response {
        self.serve_from(req, None)
    }

    /// Serves a request. `client` is included in the path-traversal log line when known.
    pub fn serve_from(&self, req: &Request, client: Option<SocketAddr>) -> Response {
        if req.method != Method::Get {
            return Response::method_not_allowed(&[Method::Get]);
        }
        let path = match self.resolve(&req.path) {
            Ok(p) => p,
            Err(ResolveError::Traversal(rule)) => {
                match client {
                    Some(c) => log::path_traversal(c, &req.path, rule),
                    None => log::warn(
                        "path_traversal",
                        &format!(
                            "path={} rule={}",
                            log::escape_value(&req.path),
                            log::escape_value(rule)
                        ),
                    ),
                }
                return Response::error(StatusCode::NotFound);
            }
            Err(ResolveError::NotFound) => return Response::error(StatusCode::NotFound),
            Err(ResolveError::Io(e)) => {
                log::error("io", &format!("resolve {}: {e}", req.path));
                return Response::error(StatusCode::InternalServerError);
            }
        };
        match fs::read(&path) {
            Ok(bytes) => Response::new(StatusCode::Ok).with_body(mime_for_path(&path), bytes),
            Err(e) if is_not_found(&e) => Response::error(StatusCode::NotFound),
            Err(e) => {
                log::error("io", &format!("read {}: {e}", log::display_path(&path).display()));
                Response::error(StatusCode::InternalServerError)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::request::{Headers, Version};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "webserver-static-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Temp dir layout: root/{index.html, a.css, sub/{index.html, x.txt}, empty/},
    /// plus a sibling file `secret.txt` next to root.
    fn setup() -> (TempDir, StaticFiles) {
        let tmp = TempDir::new();
        let root = tmp.0.join("public");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("index.html"), b"<h1>home</h1>").unwrap();
        fs::write(root.join("a.css"), b"body{}").unwrap();
        fs::write(root.join("sub").join("index.html"), b"sub index").unwrap();
        fs::write(root.join("sub").join("x.txt"), b"xx").unwrap();
        fs::write(tmp.0.join("secret.txt"), b"secret").unwrap();
        let sf = StaticFiles::new(fs::canonicalize(&root).unwrap());
        (tmp, sf)
    }

    fn req(method: Method, path: &str) -> Request {
        Request {
            method,
            path: path.to_string(),
            query: None,
            version: Version::Http11,
            headers: Headers::default(),
            body: Vec::new(),
        }
    }

    fn header<'a>(r: &'a Response, name: &str) -> Option<&'a str> {
        r.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn serves_files_and_directory_index() {
        let (_tmp, sf) = setup();
        let r = sf.serve(&req(Method::Get, "/a.css"));
        assert_eq!(r.status, StatusCode::Ok);
        assert_eq!(r.body, b"body{}");
        assert_eq!(header(&r, "content-type"), Some("text/css; charset=utf-8"));

        let r = sf.serve(&req(Method::Get, "/"));
        assert_eq!(r.status, StatusCode::Ok);
        assert_eq!(r.body, b"<h1>home</h1>");
        assert_eq!(header(&r, "content-type"), Some("text/html; charset=utf-8"));

        let r = sf.serve(&req(Method::Get, "/sub/"));
        assert_eq!(r.body, b"sub index");
        let r = sf.serve(&req(Method::Get, "/./sub//x%2Etxt"));
        assert_eq!(r.status, StatusCode::Ok);
        assert_eq!(r.body, b"xx");
    }

    #[test]
    fn missing_paths_are_404() {
        let (_tmp, sf) = setup();
        for p in ["/nope.txt", "/empty", "/empty/", "/index.html/x", "/sub/x.txt/y/z", "/%FF"] {
            let r = sf.serve(&req(Method::Get, p));
            assert_eq!(r.status, StatusCode::NotFound, "{p}");
            assert_eq!(header(&r, "content-type"), Some("text/plain; charset=utf-8"));
        }
        assert!(matches!(sf.resolve("/empty"), Err(ResolveError::NotFound)));
        assert!(matches!(sf.resolve("/index.html/x"), Err(ResolveError::NotFound)));
    }

    #[test]
    fn traversal_attempts_are_rejected() {
        let (_tmp, sf) = setup();
        let cases = [
            ("/../secret.txt", "dotdot"),
            ("/sub/%2e%2e/%2E%2E/secret.txt", "dotdot"),
            ("/..%2fsecret.txt", "dotdot"),
            ("/%5c..%5csecret.txt", "backslash"),
            ("/a\\b", "backslash"),
            ("/a%00.txt", "nul"),
            ("//server/share", "leading //"),
            ("/%2f%2fserver/share", "leading //"),
            ("/C:/Windows/win.ini", "colon"),
            ("/c%3a/x", "colon"),
            ("/index.html::$DATA", "colon"),
        ];
        for (p, rule) in cases {
            match sf.resolve(p) {
                Err(ResolveError::Traversal(r)) => assert_eq!(r, rule, "{p}"),
                other => panic!("{p}: expected Traversal({rule}), got {other:?}"),
            }
            assert_eq!(sf.serve(&req(Method::Get, p)).status, StatusCode::NotFound, "{p}");
        }
    }

    #[test]
    fn resolved_paths_stay_inside_root() {
        let (_tmp, sf) = setup();
        let p = sf.resolve("/sub/x.txt").unwrap();
        assert!(p.starts_with(sf.root()));
        assert!(p.is_file());
    }

    #[test]
    fn spec_traversal_examples_give_expected_rule() {
        let (_tmp, sf) = setup();
        let cases = [
            ("/..%2fCargo.toml", "dotdot"),
            ("/%5c..%5cwin.ini", "backslash"),
            ("/C:/Windows/win.ini", "colon"),
            ("//server/share", "leading //"),
            ("/a%00b", "nul"),
        ];
        for (p, rule) in cases {
            match sf.resolve(p) {
                Err(ResolveError::Traversal(r)) => assert_eq!(r, rule, "{p}"),
                other => panic!("{p}: expected Traversal({rule}), got {other:?}"),
            }
            let r = sf.serve(&req(Method::Get, p));
            assert_eq!(r.status, StatusCode::NotFound, "{p}");
            assert_eq!(header(&r, "content-type"), Some("text/plain; charset=utf-8"));
        }
    }

    #[test]
    fn sibling_directory_with_shared_prefix_is_outside_root() {
        let tmp = TempDir::new();
        let public = tmp.0.join("public");
        let public2 = tmp.0.join("public2");
        fs::create_dir_all(&public).unwrap();
        fs::create_dir_all(&public2).unwrap();
        fs::write(public.join("index.html"), b"ok").unwrap();
        fs::write(public2.join("secret.txt"), b"secret").unwrap();
        let root = fs::canonicalize(&public).unwrap();
        let secret = fs::canonicalize(public2.join("secret.txt")).unwrap();
        let sf = StaticFiles::new(root.clone());

        // A naive string-prefix check would accept the sibling; component-wise
        // `Path::starts_with` must not.
        assert!(secret.to_string_lossy().starts_with(&*root.to_string_lossy()));
        assert!(!secret.starts_with(&root));

        for p in [
            "/../public2/secret.txt",
            "/..%2fpublic2%2fsecret.txt",
            "/%2e%2e/public2/secret.txt",
            "/..\\public2\\secret.txt",
            "/public2/secret.txt",
            "/2/secret.txt",
            "/secret.txt",
        ] {
            match sf.resolve(p) {
                Ok(resolved) => panic!("{p} resolved to {}", resolved.display()),
                Err(ResolveError::Io(e)) => panic!("{p}: unexpected io error {e}"),
                Err(_) => {}
            }
            let r = sf.serve(&req(Method::Get, p));
            assert_eq!(r.status, StatusCode::NotFound, "{p}");
            assert_ne!(r.body, b"secret", "{p}");
        }
        assert_eq!(sf.resolve("/").unwrap(), root.join("index.html"));
    }

    #[test]
    fn served_file_has_exact_bytes_and_length() {
        let (tmp, sf) = setup();
        let bytes: Vec<u8> = (0u8..=255).chain([0, 13, 10, 0]).collect();
        fs::write(tmp.0.join("public").join("img.png"), &bytes).unwrap();
        let r = sf.serve(&req(Method::Get, "/img.png"));
        assert_eq!(r.status, StatusCode::Ok);
        assert_eq!(r.body, bytes);
        assert_eq!(header(&r, "content-type"), Some("image/png"));
        let wire = r.to_bytes();
        let text = String::from_utf8_lossy(&wire);
        assert!(text.contains(&format!("Content-Length: {}\r\n", bytes.len())), "{text}");
        assert!(wire.ends_with(&bytes));
    }

    #[test]
    fn reserved_invalid_and_oversized_names_are_404_not_500() {
        let (_tmp, sf) = setup();
        let long = format!("/{}", "a".repeat(300));
        let deep = "/a".repeat(2000);
        for p in [
            "/con", "/nul", "/aux.txt", "/COM1", "/index.html.", "/index.html%20", "/a*b",
            "/a%3Fb", "/a<b", "/a%22b", "/a|b", "/%01", "/%7f", "/...", long.as_str(),
            deep.as_str(),
        ] {
            assert!(matches!(sf.resolve(p), Err(ResolveError::NotFound)), "{p:.40}");
            assert_eq!(sf.serve(&req(Method::Get, p)).status, StatusCode::NotFound, "{p:.40}");
        }
        assert_eq!(sf.serve(&req(Method::Get, "/sub/.")).body, b"sub index");
    }

    #[test]
    fn non_get_is_405_with_allow_get() {
        let (_tmp, sf) = setup();
        for m in [Method::Post, Method::Other("DELETE".into())] {
            let r = sf.serve(&req(m, "/index.html"));
            assert_eq!(r.status, StatusCode::MethodNotAllowed);
            assert_eq!(header(&r, "allow"), Some("GET"));
        }
    }
}

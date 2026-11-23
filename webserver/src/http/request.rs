//! Request types: `Method`, `Version`, `Headers`, and `Request`.

/// HTTP request method. Variant order matters: the derived `Ord` sorts
/// `Get` before `Post`, which gives the `Allow: GET, POST` order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Method {
    Get,
    Post,
    Other(String),
}

impl Method {
    /// Case-sensitive: `"GET"` -> `Get`, `"get"` -> `Other("get")`.
    pub fn parse(token: &str) -> Method {
        match token {
            "GET" => Method::Get,
            "POST" => Method::Post,
            other => Method::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Other(s) => s,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Http10,
    Http11,
}

/// Header list. Names are stored lower-cased; insertion order is kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    /// Appends a header, lower-casing the name. Duplicates are kept.
    pub fn insert(&mut self, name: &str, value: &str) {
        self.0.push((name.to_ascii_lowercase(), value.to_string()));
    }

    /// First value for `name`, matched case-insensitively.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// All values for `name` in insertion order, matched case-insensitively.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.0
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    /// Raw, still percent-encoded, starts with '/'.
    pub path: String,
    /// Raw text after the first '?', without the '?'.
    pub query: Option<String>,
    pub version: Version,
    pub headers: Headers,
    pub body: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_parse_is_case_sensitive_and_round_trips() {
        assert_eq!(Method::parse("GET"), Method::Get);
        assert_eq!(Method::parse("POST"), Method::Post);
        assert_eq!(Method::parse("get"), Method::Other("get".into()));
        for t in ["GET", "POST", "DELETE", "get"] {
            assert_eq!(Method::parse(t).as_str(), t);
        }
    }

    #[test]
    fn method_ordering_puts_get_before_post() {
        let mut v = vec![Method::Post, Method::Get];
        v.sort();
        assert_eq!(v, vec![Method::Get, Method::Post]);
    }

    #[test]
    fn headers_lowercase_names_and_keep_order() {
        let mut h = Headers::default();
        h.insert("Content-Length", "5");
        h.insert("Host", "x");
        h.insert("content-LENGTH", "6");
        assert_eq!(h.get("CONTENT-length"), Some("5"));
        assert_eq!(h.get_all("content-length").collect::<Vec<_>>(), vec!["5", "6"]);
        assert_eq!(h.get("missing"), None);
        assert_eq!(h.0[0].0, "content-length");
        assert_eq!(h.0[2].0, "content-length");
    }
}

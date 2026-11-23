//! Exact-path router mapping `(path, method)` to a route handler.

use std::collections::{BTreeMap, HashMap};

use crate::http::request::{Method, Request};
use crate::http::response::Response;

/// A route handler: takes a parsed request and returns a response.
pub type Handler = Box<dyn Fn(&Request) -> Response + Send + Sync + 'static>;

/// Result of a router lookup.
pub enum RouteMatch<'a> {
    /// The path and method are registered.
    Found(&'a Handler),
    /// The path is registered but not for this method. `allow` is sorted (GET before POST).
    MethodNotAllowed { allow: Vec<Method> },
    /// The path is not registered; the static file handler decides.
    NotFound,
}

/// Routes are keyed by the raw (still percent-encoded) path, without the query string.
/// The router is built before the server starts and then shared read-only.
pub struct Router {
    routes: HashMap<String, BTreeMap<Method, Handler>>,
}

impl Default for Router {
    fn default() -> Self {
        Router::new()
    }
}

impl Router {
    pub fn new() -> Router {
        Router {
            routes: HashMap::new(),
        }
    }

    /// Registers a GET handler for `path`. A later registration replaces an earlier one.
    pub fn get(self, path: &str, h: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Router {
        self.add(Method::Get, path, Box::new(h))
    }

    /// Registers a POST handler for `path`. A later registration replaces an earlier one.
    pub fn post(self, path: &str, h: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Router {
        self.add(Method::Post, path, Box::new(h))
    }

    fn add(mut self, method: Method, path: &str, h: Handler) -> Router {
        self.routes
            .entry(path.to_string())
            .or_default()
            .insert(method, h);
        self
    }

    /// Looks up `path` by exact string equality, then `method`.
    pub fn lookup(&self, method: &Method, path: &str) -> RouteMatch<'_> {
        match self.routes.get(path) {
            None => RouteMatch::NotFound,
            Some(methods) => match methods.get(method) {
                Some(h) => RouteMatch::Found(h),
                None => RouteMatch::MethodNotAllowed {
                    allow: methods.keys().cloned().collect(),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::request::{Headers, Version};
    use crate::http::response::StatusCode;

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

    fn router() -> Router {
        Router::new()
            .get("/a", |_| Response::new(StatusCode::Ok))
            .post("/a", |_| Response::new(StatusCode::NotFound))
            .post("/p", |_| Response::new(StatusCode::Ok))
    }

    #[test]
    fn found_invokes_registered_handler() {
        let r = router();
        match r.lookup(&Method::Get, "/a") {
            RouteMatch::Found(h) => assert_eq!(h(&req(Method::Get, "/a")).status, StatusCode::Ok),
            _ => panic!("expected Found"),
        }
        match r.lookup(&Method::Post, "/a") {
            RouteMatch::Found(h) => {
                assert_eq!(h(&req(Method::Post, "/a")).status, StatusCode::NotFound)
            }
            _ => panic!("expected Found"),
        }
    }

    #[test]
    fn method_not_allowed_lists_sorted_methods() {
        let r = router();
        match r.lookup(&Method::Get, "/p") {
            RouteMatch::MethodNotAllowed { allow } => assert_eq!(allow, vec![Method::Post]),
            _ => panic!("expected MethodNotAllowed"),
        }
        match r.lookup(&Method::Other("DELETE".into()), "/a") {
            RouteMatch::MethodNotAllowed { allow } => {
                assert_eq!(allow, vec![Method::Get, Method::Post])
            }
            _ => panic!("expected MethodNotAllowed"),
        }
    }

    #[test]
    fn unknown_or_non_exact_path_is_not_found() {
        let r = router();
        assert!(matches!(r.lookup(&Method::Get, "/missing"), RouteMatch::NotFound));
        assert!(matches!(r.lookup(&Method::Get, "/a/"), RouteMatch::NotFound));
        assert!(matches!(r.lookup(&Method::Get, "/%61"), RouteMatch::NotFound));
        assert!(matches!(Router::default().lookup(&Method::Get, "/a"), RouteMatch::NotFound));
    }
}

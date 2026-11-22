//! The request and response as the handlers see them, and the few pieces of
//! Go's net/http and net/url that decide what a request means: how a query
//! string splits, how a path unescapes, how a path is cleaned.

use crate::encode::Json;
use std::io::{self, Read};

pub struct Req<'a> {
    pub method: String,
    /// The path as sent, still escaped — what Go's mux matches on.
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    /// The Host the request named, which is where links point when no --site
    /// overrides it.
    pub host: String,
    /// The client's IP: the only identity a keyless caller has.
    pub remote: String,
    pub body: &'a mut dyn Read,
}

impl Req<'_> {
    /// The first value of a header, trimmed, as `http.Header.Get` answers.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim())
    }

    /// `r.URL.Query().Get(name)`.
    pub fn query_get(&self, name: &str) -> String {
        for pair in self.query.split('&') {
            if pair.is_empty() || pair.contains(';') {
                continue;
            }
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if let (Some(k), Some(v)) = (unescape(k, true), unescape(v, true))
                && k == name
            {
                return v;
            }
        }
        String::new()
    }

    /// Up to `limit` bytes of the body — `io.LimitReader`, which truncates
    /// rather than refusing.
    pub fn read_body(&mut self, limit: u64) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        self.body.take(limit).read_to_end(&mut out)?;
        Ok(out)
    }
}

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
    /// Run if the response could not be written: the one-shot key delivery
    /// puts the key back, since a key nobody received was not collected.
    pub undo: Option<Box<dyn FnOnce() + Send>>,
}

impl Resp {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Resp {
        Resp { status, headers: vec![("Content-Type", content_type.to_owned())], body: body.into(), undo: None }
    }

    pub fn empty(status: u16) -> Resp {
        Resp { status, headers: vec![], body: vec![], undo: None }
    }

    pub fn json(status: u16, body: &Json) -> Resp {
        Resp::new(status, "application/json", body.encode())
    }

    pub fn html(status: u16, body: String) -> Resp {
        Resp::new(status, "text/html; charset=utf-8", body)
    }

    /// Object storage's error shape, not the registry's: R2 and S3 speak XML,
    /// so a client cannot get away with assuming every failure is an envelope.
    pub fn xml(status: u16, code: &str) -> Resp {
        Resp::new(
            status,
            "application/xml",
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{code}</Code></Error>"),
        )
    }
}

/// Percent-decoding, as `url.QueryUnescape` (`query`, where `+` is a space) or
/// `url.PathUnescape`. None for a malformed escape.
pub fn unescape(s: &str, query: bool) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                    return None;
                }
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
                continue;
            }
            b'+' if query => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Whether Go's server would have parsed this request-target at all: origin
/// form (`/…`) or absolute form (`scheme://…`), no control bytes anywhere, and
/// every escape in the path well formed. `GET None` or `GET *` is a 400 there,
/// not a redirect to `/None`.
pub fn parseable(path: &str, query: &str) -> bool {
    let clean = |s: &str| !s.bytes().any(|c| c < 0x20 || c == 0x7f);
    let form = path.starts_with('/') || path.contains("://");
    form && clean(path) && clean(query) && unescape(path, false).is_some()
}

/// Go's `path.Clean`.
fn path_clean(p: &str) -> String {
    let rooted = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_owned(),
        (false, false) => joined,
    }
}

/// net/http's `cleanPath`: `path.Clean`, rooted, keeping a trailing slash.
pub fn clean_path(p: &str) -> String {
    if p.is_empty() {
        return "/".to_owned();
    }
    let p = if p.starts_with('/') { p.to_owned() } else { format!("/{p}") };
    let mut np = path_clean(&p);
    if p.ends_with('/') && np != "/" {
        np.push('/');
    }
    np
}

/// The 307 Go's mux answers for a path it would rather have been sent
/// differently, body and all.
pub fn redirect(method: &str, path: &str, query: &str) -> Resp {
    let mut url = String::new();
    for b in path.bytes() {
        if b >= 0x80 {
            url.push_str(&format!("%{b:02X}"));
        } else {
            url.push(b as char);
        }
    }
    if !query.is_empty() {
        url.push('?');
        url.push_str(query);
    }
    let mut resp = Resp::empty(307);
    resp.headers.push(("Location", url.clone()));
    if method == "GET" || method == "HEAD" {
        resp.headers.push(("Content-Type", "text/html; charset=utf-8".to_owned()));
    }
    if method == "GET" {
        let href = url
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&#34;")
            .replace('\'', "&#39;");
        resp.body = format!("<a href=\"{href}\">Temporary Redirect</a>.\n\n").into_bytes();
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_that_is_neither_origin_nor_absolute_form_does_not_parse() {
        assert!(parseable("/v1/key", ""));
        assert!(parseable("http://localhost/v1/key", ""));
        assert!(!parseable("None", ""));
        assert!(!parseable("*", ""));
    }

    #[test]
    fn paths_clean_the_way_go_cleans_them() {
        assert_eq!(clean_path(""), "/");
        assert_eq!(clean_path("//v1//key"), "/v1/key");
        assert_eq!(clean_path("/a/./b/../c/"), "/a/c/");
        assert_eq!(clean_path("/../x"), "/x");
        assert_eq!(clean_path("/_storage/"), "/_storage/");
    }

    #[test]
    fn unescaping_refuses_a_broken_escape() {
        assert_eq!(unescape("a%41+b", true).as_deref(), Some("aA b"));
        assert_eq!(unescape("a+b", false).as_deref(), Some("a+b"));
        assert_eq!(unescape("%4", false), None);
        assert_eq!(unescape("%zz", false), None);
    }
}

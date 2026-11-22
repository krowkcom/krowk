//! The card page at /a/{slug}, and the bare HTML every page here is made of.
//!
//! Deliberately plain: an unfurler fetches the page once with no credentials,
//! reads the OpenGraph tags and runs nothing, so there is no styling or script
//! worth having.

use crate::http::{Req, Resp};
use crate::store::{App, DEFAULT_VISIBILITY, SHARED_VISIBILITY, share_readable};
use crate::view::share_url;

/// Go's `html.EscapeString`.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            c => out.push(c),
        }
    }
    out
}

/// A title, the meta tags sorted so a test can pin them, and a body.
pub fn page(title: &str, tags: &[(&str, &str)], body: &str) -> String {
    let mut tags = tags.to_vec();
    tags.sort();
    let meta: String = tags
        .iter()
        .map(|(k, v)| format!("<meta property=\"{}\" content=\"{}\">\n", escape(k), escape(v)))
        .collect();
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>{}</title>\n{meta}</head>\n<body>\n{body}\n</body>\n</html>\n",
        escape(title)
    )
}

/// The size as a card says it. Not shared with the CLI's own: the stand-in is
/// what the CLI is tested against, not something it is built on.
fn human_bytes(n: i64) -> String {
    const UNIT: i64 = 1024;
    if n < UNIT {
        return format!("{n} B");
    }
    let (mut div, mut exp) = (UNIT, 0);
    while n / div >= UNIT && exp < 3 {
        div *= UNIT;
        exp += 1;
    }
    format!("{:.0} {}", n as f64 / div as f64, ["KB", "MB", "GB", "TB"][exp])
}

/// Public and keyless, because the slug is the capability. Anything that is
/// not public reads as never minted — existence itself is the secret — except
/// a shared card opened with its live token.
pub fn artifact_page(app: &App, req: &Req, slug: &str) -> Resp {
    let s = app.lock();
    let a = s
        .artifacts
        .get(slug)
        .filter(|a| a.visibility == DEFAULT_VISIBILITY || share_readable(a, &req.query_get("share")));
    let Some(a) = a else {
        return Resp::html(404, page("Not found", &[], "<p>No such artifact.</p>"));
    };

    let gone = if a.deleted_at.is_some() {
        Some("taken_down")
    } else if s.expired(a) {
        Some("expired")
    } else {
        None
    };
    // A shared card names the link that reaches it: the bare page is a 404.
    let card_url = if a.visibility == SHARED_VISIBILITY && !a.share_token.is_empty() {
        share_url(&a.url, &a.share_token)
    } else {
        a.url.clone()
    };
    let filename = a.filename.as_str();

    // 410 rather than 404: the link is pasted somewhere, and "this existed and
    // is gone" is a different thing to tell its reader. A takedown does not
    // name its file.
    if let Some(gone) = gone {
        let (title, body) = if gone == "expired" {
            (filename.to_owned(), format!("<p>{} has expired.</p>", escape(filename)))
        } else {
            ("Taken down".to_owned(), "<p>This artifact was taken down.</p>".to_owned())
        };
        let tags = [("og:title", title.as_str()), ("og:type", "website"), ("og:url", card_url.as_str())];
        return Resp::html(410, page(&title, &tags, &body));
    }

    // A pending card says so, rather than naming bytes in og:image that would be
    // a broken image in every card built from it.
    if a.state != "ready" {
        let tags = [
            ("og:title", filename),
            ("og:type", "website"),
            ("og:url", card_url.as_str()),
            ("og:description", "Upload pending — the bytes have not landed yet."),
        ];
        return Resp::html(200, page(filename, &tags, &format!("<p>{} — upload pending.</p>", escape(filename))));
    }

    let description = format!("{} · krowk", human_bytes(a.byte_size));
    let mut tags = vec![
        ("og:title", filename),
        ("og:type", "website"),
        ("og:url", card_url.as_str()),
        ("og:description", description.as_str()),
    ];
    let mut body = format!("<p><a href=\"{}\">{}</a></p>", escape(&a.file_url), escape(filename));
    // og:image names the bytes, never this page: an unfurler fetching it expects
    // image bytes back.
    if a.content_type.starts_with("image/") {
        tags.push(("og:image", &a.file_url));
        tags.push(("twitter:card", "summary_large_image"));
        body = format!("<p><img src=\"{}\" alt=\"{}\"></p>{body}", escape(&a.file_url), escape(filename));
    }
    Resp::html(200, page(filename, &tags, &body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_the_way_a_card_says_them() {
        assert_eq!(human_bytes(5), "5 B");
        assert_eq!(human_bytes(1536), "2 KB");
        assert_eq!(human_bytes(2560), "2 KB");
        assert_eq!(human_bytes(3 << 20), "3 MB");
    }
}

//! Write-time markdown rendering and sanitization.
//!
//! # Threat model
//!
//! Client-generated HTML persisted and served to other users is stored XSS. The client is the
//! attacker, so sanitizing happens server-side, always.
//!
//! Two independent things are true here, and both matter:
//!
//! 1. `pulldown-cmark` passes raw HTML in the source through to its output verbatim. It is a
//!    markdown parser, not a sanitizer, and must never be trusted as one.
//! 2. `ammonia` is therefore the sole authority on what reaches the database. Everything
//!    goes through [`sanitize`]; nothing bypasses it.
//!
//! The allowlist below is deliberately tight. It is easier to add a tag on request than to
//! discover which of forty tags was the one that let script through.

use std::collections::HashSet;
use std::sync::OnceLock;

/// Build the sanitizer allowlist.
///
/// Rebuilt once per isolate and cached: `ammonia::Builder` construction allocates several
/// hash sets, and doing that per post would waste a measurable slice of the CPU budget.
fn sanitizer() -> &'static ammonia::Builder<'static> {
    static SANITIZER: OnceLock<ammonia::Builder<'static>> = OnceLock::new();
    SANITIZER.get_or_init(|| {
        let mut b = ammonia::Builder::empty();
        b.tags(HashSet::from([
            // Block
            "p",
            "br",
            "hr",
            "blockquote",
            "pre",
            "div",
            "ul",
            "ol",
            "li",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "table",
            "thead",
            "tbody",
            "tr",
            "th",
            "td",
            // Inline
            "a",
            "code",
            "em",
            "strong",
            "del",
            "sup",
            "sub",
            "span",
            "img",
        ]))
        .link_rel(Some("nofollow ugc noopener noreferrer"))
        // Only these schemes may appear in href/src. `javascript:` and `data:` are absent
        // by construction, which is the point.
        .url_schemes(HashSet::from(["http", "https", "mailto"]));
        b.tag_attributes(std::collections::HashMap::from([
            ("a", HashSet::from(["href", "title"])),
            ("img", HashSet::from(["src", "alt", "title"])),
            ("td", HashSet::from(["align"])),
            ("th", HashSet::from(["align"])),
            // Syntax highlighting hooks emitted by the code-block renderer.
            ("code", HashSet::from(["class"])),
            ("span", HashSet::from(["class"])),
        ]));
        b
    })
}

/// Sanitize a fragment of HTML against the allowlist.
///
/// This is the only path by which HTML may reach storage.
pub fn sanitize(html: &str) -> String {
    sanitizer().clean(html).to_string()
}

fn options() -> pulldown_cmark::Options {
    use pulldown_cmark::Options;
    let mut o = Options::empty();
    o.insert(Options::ENABLE_STRIKETHROUGH);
    o.insert(Options::ENABLE_TABLES);
    o.insert(Options::ENABLE_FOOTNOTES);
    o.insert(Options::ENABLE_TASKLISTS);
    // Deliberately NOT enabled: ENABLE_SMART_PUNCTUATION (mangles code discussion),
    // ENABLE_HEADING_ATTRIBUTES (lets authors inject id/class into the page).
    o
}

/// Render markdown to sanitized HTML. Runs once per post, at write time.
///
/// The output is safe to emit verbatim into a page; that is the whole contract.
pub fn markdown_to_html(md: &str) -> String {
    let parser = pulldown_cmark::Parser::new_ext(md, options());
    // Markdown expands to roughly its own size in HTML; pre-sizing avoids a few reallocs
    // on the hot write path.
    let mut raw = String::with_capacity(md.len() + md.len() / 2);
    pulldown_cmark::html::push_html(&mut raw, parser);
    sanitize(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_basic_markdown() {
        let html = markdown_to_html("# Title\n\nSome **bold** and `code`.");
        assert!(html.contains("<h1>Title</h1>"), "got: {html}");
        assert!(html.contains("<strong>bold</strong>"), "got: {html}");
        assert!(html.contains("<code>code</code>"), "got: {html}");
    }

    #[test]
    fn strips_script_tags() {
        let html = markdown_to_html("hello <script>alert('xss')</script> world");
        assert!(!html.contains("script"), "got: {html}");
        assert!(!html.contains("alert"), "got: {html}");
    }

    #[test]
    fn strips_event_handlers() {
        let html = markdown_to_html(r#"<img src="x" onerror="alert(1)">"#);
        assert!(!html.contains("onerror"), "got: {html}");
        assert!(!html.contains("alert"), "got: {html}");
    }

    #[test]
    fn strips_javascript_urls() {
        for src in [
            "[click](javascript:alert(1))",
            r#"<a href="javascript:alert(1)">click</a>"#,
            r#"<a href="JaVaScRiPt:alert(1)">click</a>"#,
        ] {
            let html = markdown_to_html(src);
            assert!(
                !html.to_ascii_lowercase().contains("javascript"),
                "{src} -> {html}"
            );
        }
    }

    #[test]
    fn strips_data_urls() {
        let html = markdown_to_html(r#"<img src="data:text/html;base64,PHNjcmlwdD4=">"#);
        assert!(!html.contains("data:"), "got: {html}");
    }

    #[test]
    fn strips_style_and_iframe_and_form() {
        for src in [
            "<style>body{display:none}</style>",
            r#"<iframe src="https://evil.example"></iframe>"#,
            r#"<form action="https://evil.example"><input name="pw"></form>"#,
            "<object data=\"x\"></object>",
            "<svg><use href=\"#x\"/></svg>",
        ] {
            let html = markdown_to_html(src);
            for bad in ["style", "iframe", "form", "input", "object", "svg"] {
                assert!(!html.contains(bad), "{src} -> {html}");
            }
        }
    }

    #[test]
    fn keeps_safe_links_and_adds_rel() {
        let html = markdown_to_html("[notespace](https://notespace.org)");
        assert!(
            html.contains(r#"href="https://notespace.org""#),
            "got: {html}"
        );
        assert!(html.contains("nofollow"), "got: {html}");
        assert!(html.contains("noopener"), "got: {html}");
    }

    #[test]
    fn escapes_html_entities_in_text() {
        let html = markdown_to_html("5 < 6 && 7 > 6");
        assert!(!html.contains("< 6"), "raw angle bracket survived: {html}");
        assert!(
            html.contains("&lt;") || html.contains("&amp;lt;"),
            "got: {html}"
        );
    }

    #[test]
    fn sanitize_is_idempotent() {
        // Re-sanitizing stored HTML must not corrupt it; edits re-render and re-store.
        let once = markdown_to_html("[a](https://example.com) **b** `c`");
        let twice = sanitize(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn handles_empty_and_whitespace() {
        assert_eq!(markdown_to_html("").trim(), "");
        assert_eq!(markdown_to_html("   \n\n  ").trim(), "");
    }

    #[test]
    fn renders_tables_and_strikethrough() {
        let html = markdown_to_html("| a | b |\n|---|---|\n| 1 | 2 |\n\n~~gone~~");
        assert!(html.contains("<table>"), "got: {html}");
        assert!(html.contains("<del>gone</del>"), "got: {html}");
    }

    #[test]
    fn does_not_panic_on_deeply_nested_input() {
        // Guards against stack exhaustion, which on wasm is an unrecoverable trap.
        let md = "> ".repeat(500) + "deep";
        let _ = markdown_to_html(&md);
        let md = "*".repeat(2000);
        let _ = markdown_to_html(&md);
    }
}

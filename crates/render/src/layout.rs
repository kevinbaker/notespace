//! The page shell: one `<head>`, one stylesheet, one header. Every page is this around a body.
//!
//! The stylesheet and the fonts live in `crates/render/public/static/`, which Cloudflare serves
//! as static assets in front of the Worker: those requests never invoke it and do not count
//! against the free plan's daily cap, and `_headers` marks them immutable. The page links the
//! sheet with `?v=BAKE_REVISION`, so a change to it is a new URL. A space with a theme links a
//! second sheet of its own, `/s/{path}/theme.css?v={hash}`, built by [`theme_css`].

use maud::{html, Markup, DOCTYPE};
use notespace_core::theme::Theme;

/// The site stylesheet, verbatim. Served as a static asset; included here for the tests, and
/// as part of [`crate::BAKE_REVISION`].
pub const STYLE: &str = include_str!("../public/static/style.css");

/// The one font file worth preloading: roman, Latin. The rest load as the text needs them.
pub const PRELOAD_FONT: &str = "/static/fonts/noto-sans-roman-latin.woff2";

/// A space's look, for the page that belongs to it.
pub struct SpaceTheme<'a> {
    /// `/s/sports/hockey`, no trailing slash.
    pub url: String,
    pub theme: &'a Theme,
}

impl SpaceTheme<'_> {
    /// Where the space's stylesheet is, or nothing when it has none.
    pub fn href(&self) -> Option<String> {
        (!self.theme.is_empty())
            .then(|| format!("{}/theme.css?v={}", self.url, self.theme.version()))
    }
}

/// How wide the main column is.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Width {
    /// Reading: threads, lists, profiles.
    #[default]
    Normal,
    /// A single form.
    Narrow,
    /// Tables and queues.
    Wide,
}

/// Everything around the body. `Default` is a plain page titled "notespace".
#[derive(Default)]
pub struct Shell<'a> {
    /// Goes in `<title>`; the site name is appended unless this is it.
    pub title: &'a str,
    pub width: Width,
    /// After the brand in the header: `a` elements, one per segment, or text for the current one.
    pub crumbs: Markup,
    /// Right-hand header links particular to this page (rss, modlog); the account links follow.
    pub links: Markup,
    /// Anything else for `<head>`: an RSS `<link>`, say.
    pub head: Markup,
    /// The space's look, if the page belongs to one.
    pub theme: Option<SpaceTheme<'a>>,
    /// After the body: a `<script defer>` that enhances it, or nothing.
    pub tail: Markup,
}

impl Shell<'_> {
    pub fn render(self, body: Markup) -> Markup {
        html! {
            (DOCTYPE)
            html lang="en" {
                head {
                    meta charset="utf-8";
                    meta name="viewport" content="width=device-width, initial-scale=1";
                    link rel="icon" href="data:,";
                    title {
                        @if self.title.is_empty() || self.title == "notespace" { "notespace" }
                        @else { (self.title) " — notespace" }
                    }
                    (self.head)
                    link rel="preload" as="font" type="font/woff2" crossorigin href=(PRELOAD_FONT);
                    link rel="stylesheet" href={ "/static/style.css?v=" (revision()) };
                    @if let Some(href) = self.theme.as_ref().and_then(SpaceTheme::href) {
                        link rel="stylesheet" href=(href);
                    }
                }
                body {
                    header class="site" {
                        nav class="crumbs" {
                            a class="brand" href="/" { "notespace" }
                            (self.crumbs)
                        }
                        // Identical for every reader: baked pages are shared byte-for-byte, so
                        // "signed in as …" cannot live here. /settings asks for sign-in if needed.
                        nav class="links" {
                            (self.links)
                            a href="/login" { "sign in" }
                            a href="/register" { "register" }
                            a href="/settings" { "account" }
                        }
                    }
                    main class=(match self.width {
                        Width::Normal => "",
                        Width::Narrow => "narrow",
                        Width::Wide => "wide",
                    }) { (body) }
                    (self.tail)
                }
            }
        }
    }
}

/// A crumb trail: each ancestor a link, the last one plain text. Separators are the header's
/// gap, so the markup is just the segments.
pub fn crumbs<'a>(segments: impl IntoIterator<Item = (&'a str, Option<String>)>) -> Markup {
    html! {
        @for (label, href) in segments {
            span aria-hidden="true" { "/" }
            @match href {
                Some(h) => a href=(h) { (label) },
                None => span { (label) },
            }
        }
    }
}

/// [`crate::BAKE_REVISION`] as it appears in URLs.
fn revision() -> String {
    format!("{:016x}", crate::BAKE_REVISION)
}

/// A space's stylesheet: its custom-property overrides -- `both` for either colour scheme,
/// `light` and `dark` under a media query -- then its own CSS. Every value passed [`Theme`]'s
/// validation, which is what makes serving it safe.
pub fn theme_css(t: &Theme) -> String {
    let mut out = String::new();
    block(&mut out, "", &t.both);
    block(&mut out, "@media(prefers-color-scheme:light)", &t.light);
    block(&mut out, "@media(prefers-color-scheme:dark)", &t.dark);
    if !t.css.is_empty() {
        out.push('\n');
        out.push_str(&t.css);
        out.push('\n');
    }
    out
}

fn block(out: &mut String, wrap: &str, decls: &[(String, String)]) {
    if decls.is_empty() {
        return;
    }
    out.push_str(wrap);
    if !wrap.is_empty() {
        out.push('{');
    }
    out.push_str(":root{");
    for (n, v) in decls {
        out.push_str("--");
        out.push_str(n);
        out.push(':');
        out.push_str(v);
        out.push(';');
    }
    out.push('}');
    if !wrap.is_empty() {
        out.push('}');
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_space_sheet_is_linked_after_the_site_sheet_and_only_when_there_is_one() {
        let t = Theme::parse_lines("accent: #c00").unwrap();
        let html = Shell {
            theme: Some(SpaceTheme {
                url: "/s/sports/hockey".into(),
                theme: &t,
            }),
            ..Default::default()
        }
        .render(html! { p { "x" } })
        .into_string();
        let site = html.find("/static/style.css?v=").unwrap();
        let space = html
            .find(&format!("/s/sports/hockey/theme.css?v={}", t.version()))
            .unwrap();
        assert!(
            space > site,
            "the space's sheet must come after the site's to win"
        );
        assert!(html.contains(r#"rel="preload" as="font""#));
        assert!(!html.contains("<style"), "nothing is inlined");
        let none = Shell {
            theme: Some(SpaceTheme {
                url: "/s/general".into(),
                theme: &Theme::default(),
            }),
            ..Default::default()
        }
        .render(html! {})
        .into_string();
        assert!(!none.contains("theme.css"));
    }

    #[test]
    fn the_theme_sheet_scopes_its_sections_and_appends_the_custom_css() {
        let t = Theme::parse_lines("accent: #c00\nlight.bg: #fff\ndark.bg: #000")
            .unwrap()
            .with_css(".site{border-bottom:3px solid var(--accent)}")
            .unwrap();
        let css = theme_css(&t);
        assert!(css.starts_with(":root{--accent:#c00;}\n"), "{css}");
        assert!(css.contains("@media(prefers-color-scheme:light){:root{--bg:#fff;}}"));
        assert!(css.contains("@media(prefers-color-scheme:dark){:root{--bg:#000;}}"));
        assert!(css
            .trim_end()
            .ends_with(".site{border-bottom:3px solid var(--accent)}"));
    }

    #[test]
    fn the_title_carries_the_site_name_once() {
        let html = Shell {
            title: "Hello",
            ..Default::default()
        }
        .render(html! {})
        .into_string();
        assert!(html.contains("<title>Hello — notespace</title>"));
        let home = Shell::default().render(html! {}).into_string();
        assert!(home.contains("<title>notespace</title>"));
    }

    #[test]
    fn the_stylesheet_uses_no_literal_colours_outside_the_token_block() {
        // Everything past the tokens is written in terms of them, or a theme cannot reach it.
        let body = STYLE.split("/* Base */").nth(1).expect("a Base section");
        assert!(
            STYLE.contains("--font:\"Noto Sans\""),
            "the face is a token"
        );
        for line in body.lines() {
            let l = line.trim();
            if l.starts_with("/*") || l.is_empty() {
                continue;
            }
            assert!(
                !l.contains('#'),
                "literal colour outside the token block: {l}"
            );
        }
    }
}

//! A space's look, the way a subreddit has one: not a stylesheet but a set of values for the
//! custom properties the one site stylesheet is built from. `accent`, `bg`, `font`, `size`,
//! `measure` and the rest are declared once in `crates/render/style.css`; a theme overrides
//! any of them, for both colour schemes or for one.
//!
//! A theme may also carry a stylesheet of its own, for the things tokens cannot say: a banner,
//! a different post layout, a flourish on the header. It is served as the space's own `.css`
//! file, never inlined into a page.
//!
//! Values are validated here so that the render crate can serve them without a second look: a
//! theme is authored by a space's admin, and the pages it lands in are shared by every reader.

use serde_json::{Map, Value};

/// Longest a single value may be. Long enough for a font stack, too short for mischief.
pub const MAX_VALUE_LEN: usize = 120;
/// Most declarations a theme may hold across all three sections.
pub const MAX_DECLARATIONS: usize = 64;
/// Longest a space's own stylesheet may be. Room for a real skin; not for a copy of Bootstrap.
pub const MAX_CSS_LEN: usize = 16 * 1024;

/// CSS functions a value may call. Anything that could fetch (`url`, `image-set`, `src`) is
/// absent, and so is anything not in this list.
const FUNCTIONS: &[&str] = &[
    "rgb",
    "rgba",
    "hsl",
    "hsla",
    "oklch",
    "oklab",
    "lab",
    "lch",
    "color",
    "color-mix",
    "calc",
    "min",
    "max",
    "clamp",
    "var",
];

/// Property overrides, as `(name, value)` pairs with the `--` left off. `both` applies in
/// either colour scheme; `light` and `dark` in one. `css` is the space's own stylesheet,
/// applied after the overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Theme {
    pub both: Vec<(String, String)>,
    pub light: Vec<(String, String)>,
    pub dark: Vec<(String, String)>,
    pub css: String,
}

impl Theme {
    /// The theme in `space.config`, which is JSON of the shape
    /// `{"theme": {"accent": "#0a7a45", "dark": {"accent": "#3fbf7f"}, "css": ".site{…}"}}`.
    /// Anything that does
    /// not validate is dropped silently: a stored theme was checked on the way in, and a page
    /// is not the place to report on one that was not.
    pub fn from_config(config: &str) -> Theme {
        serde_json::from_str::<Value>(config)
            .ok()
            .and_then(|v| v.get("theme").cloned())
            .and_then(|t| Theme::from_json(&t).ok())
            .unwrap_or_default()
    }

    /// The `"theme"` object itself.
    pub fn from_json(v: &Value) -> Result<Theme, String> {
        let Some(obj) = v.as_object() else {
            return Err("the theme must be an object".into());
        };
        let mut theme = Theme::default();
        for (k, v) in obj {
            match (k.as_str(), v) {
                ("light", Value::Object(m)) => theme.light = section(m)?,
                ("dark", Value::Object(m)) => theme.dark = section(m)?,
                ("light" | "dark", _) => return Err(format!("{k} must be an object")),
                ("css", Value::String(css)) => theme.css = checked_css(css)?,
                ("css", _) => return Err("css must be a string".into()),
                (name, Value::String(value)) => theme.both.push(declaration(name, value)?),
                (name, _) => return Err(format!("{name} must be a string")),
            }
        }
        theme.check_size()?;
        Ok(theme)
    }

    /// The form an admin types: one `name: value` per line, with `light.` or `dark.` in front
    /// of a name to scope it. Blank lines and `#` comments are skipped.
    pub fn parse_lines(text: &str) -> Result<Theme, String> {
        let mut theme = Theme::default();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, value)) = line.split_once(':') else {
                return Err(format!("line {}: expected `name: value`", i + 1));
            };
            let name = name.trim();
            let (section, name) = match name.split_once('.') {
                Some(("light", n)) => (&mut theme.light, n),
                Some(("dark", n)) => (&mut theme.dark, n),
                Some((other, _)) => {
                    return Err(format!(
                        "line {}: `{other}.` is not a section; use `light.` or `dark.`",
                        i + 1
                    ))
                }
                None => (&mut theme.both, name),
            };
            let d = declaration(name, value.trim()).map_err(|e| format!("line {}: {e}", i + 1))?;
            section.push(d);
        }
        theme.check_size()?;
        Ok(theme)
    }

    /// The same theme with a stylesheet of its own, or why the stylesheet was refused.
    pub fn with_css(mut self, css: &str) -> Result<Theme, String> {
        self.css = checked_css(css.trim())?;
        Ok(self)
    }

    /// The inverse of [`Theme::parse_lines`], for putting a stored theme back in the form.
    pub fn to_lines(&self) -> String {
        let mut out = String::new();
        for (prefix, decls) in [
            ("", &self.both),
            ("light.", &self.light),
            ("dark.", &self.dark),
        ] {
            for (n, v) in decls {
                out.push_str(prefix);
                out.push_str(n);
                out.push_str(": ");
                out.push_str(v);
                out.push('\n');
            }
        }
        out
    }

    /// The `"theme"` value to store in `space.config`.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        for (n, v) in &self.both {
            m.insert(n.clone(), Value::String(v.clone()));
        }
        for (key, decls) in [("light", &self.light), ("dark", &self.dark)] {
            if !decls.is_empty() {
                let sub: Map<String, Value> = decls
                    .iter()
                    .map(|(n, v)| (n.clone(), Value::String(v.clone())))
                    .collect();
                m.insert(key.into(), Value::Object(sub));
            }
        }
        if !self.css.is_empty() {
            m.insert("css".into(), Value::String(self.css.clone()));
        }
        Value::Object(m)
    }

    pub fn is_empty(&self) -> bool {
        self.both.is_empty() && self.light.is_empty() && self.dark.is_empty() && self.css.is_empty()
    }

    /// Changes whenever the theme does: the cache-busting part of its stylesheet's URL, so the
    /// file can be served as immutable.
    pub fn version(&self) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.to_json().to_string().bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{h:016x}")
    }

    fn check_size(&self) -> Result<(), String> {
        let n = self.both.len() + self.light.len() + self.dark.len();
        if n > MAX_DECLARATIONS {
            return Err(format!(
                "a theme may set at most {MAX_DECLARATIONS} properties"
            ));
        }
        Ok(())
    }
}

fn section(m: &Map<String, Value>) -> Result<Vec<(String, String)>, String> {
    m.iter()
        .map(|(k, v)| match v {
            Value::String(s) => declaration(k, s),
            _ => Err(format!("{k} must be a string")),
        })
        .collect()
}

/// One validated `name: value`.
fn declaration(name: &str, value: &str) -> Result<(String, String), String> {
    check_name(name)?;
    check_value(value).map_err(|e| format!("{name}: {e}"))?;
    Ok((name.to_string(), value.to_string()))
}

/// A custom-property name without its `--`: lowercase, digits and hyphens, starting with a
/// letter. `light` and `dark` are section names and not properties.
fn check_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok = matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name.len() <= 32;
    if !ok {
        return Err(format!(
            "`{name}` is not a property name (lowercase letters, digits and hyphens)"
        ));
    }
    if name == "light" || name == "dark" {
        return Err(format!("`{name}` is a section, not a property"));
    }
    Ok(())
}

/// Colours, lengths, numbers, keywords, quoted font names and the functions in [`FUNCTIONS`].
/// Nothing that could close the declaration or the `<style>` element, and nothing that fetches.
fn check_value(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("empty value".into());
    }
    if value.len() > MAX_VALUE_LEN {
        return Err(format!("longer than {MAX_VALUE_LEN} characters"));
    }
    if let Some(c) = value
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || " #%.,()'\"_-".contains(*c)))
    {
        return Err(format!("`{c}` is not allowed in a value"));
    }
    // Every `(` must belong to a listed function, and every `(` must close.
    let mut depth = 0i32;
    for (i, c) in value.char_indices() {
        match c {
            '(' => {
                depth += 1;
                let head = &value[..i];
                let start = head
                    .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                    .map_or(0, |p| p + 1);
                let func = &head[start..];
                if !FUNCTIONS.contains(&func) {
                    return Err(format!("`{func}(` is not an allowed function"));
                }
            }
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err("unbalanced parentheses".into());
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err("unbalanced parentheses".into());
    }
    Ok(())
}

/// A space's own stylesheet. It is served as its own `text/css` response and never inlined, so
/// the concern is not escaping a `<style>` but what CSS itself can do: `<` has no business in
/// a stylesheet, `@import` and `expression()` are refused by name, and the length is capped.
/// `url()` is allowed on purpose -- a banner is the first thing anyone skins -- and the site's
/// CSP decides where it may point.
fn checked_css(css: &str) -> Result<String, String> {
    if css.len() > MAX_CSS_LEN {
        return Err(format!("the stylesheet is longer than {MAX_CSS_LEN} bytes"));
    }
    if css.contains('<') {
        return Err("`<` is not allowed in the stylesheet".into());
    }
    let lower = css.to_ascii_lowercase();
    for banned in ["@import", "expression(", "-moz-binding", "behavior:"] {
        if lower.contains(banned) {
            return Err(format!("`{banned}` is not allowed in the stylesheet"));
        }
    }
    Ok(css.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_theme_round_trips_through_lines_and_json() {
        let t = Theme::parse_lines(
            "# the look\naccent: #0a7a45\nfont: \"Inter\", system-ui, sans-serif\n\
             dark.accent: #3fbf7f\nlight.bg: rgb(250 250 250)\n",
        )
        .unwrap();
        assert_eq!(t.both.len(), 2);
        assert_eq!(t.dark, vec![("accent".to_string(), "#3fbf7f".to_string())]);
        assert_eq!(t.light[0].0, "bg");
        let json = serde_json::json!({ "theme": t.to_json() }).to_string();
        assert_eq!(Theme::from_config(&json), t);
        assert_eq!(Theme::parse_lines(&t.to_lines()).unwrap(), t);
    }

    #[test]
    fn a_stylesheet_rides_along_and_changes_the_version() {
        let plain = Theme::parse_lines("accent: #c00").unwrap();
        let skinned = plain
            .clone()
            .with_css(".site{border-bottom:3px solid var(--accent)}")
            .unwrap();
        assert_ne!(plain.version(), skinned.version());
        assert_eq!(
            plain.version(),
            Theme::parse_lines("accent: #c00").unwrap().version()
        );
        let json = serde_json::json!({ "theme": skinned.to_json() }).to_string();
        assert_eq!(Theme::from_config(&json), skinned);
        assert!(Theme::default().with_css("").unwrap().is_empty());
        assert!(!Theme::default().with_css("body{}").unwrap().is_empty());
        for bad in [
            "</style><script>1</script>",
            "@import url(x)",
            "@IMPORT 'x'",
            "a{width:expression(1)}",
            &"a{}".repeat(6000),
        ] {
            assert!(
                Theme::default().with_css(bad).is_err(),
                "accepted {bad:.40}"
            );
        }
        // Backgrounds are the point of a skin.
        assert!(Theme::default()
            .with_css(".site{background:url(https://x/y.png)}")
            .is_ok());
    }

    #[test]
    fn nothing_that_could_escape_the_style_element_or_fetch_is_accepted() {
        for bad in [
            "accent: red;}</style><script>",
            "accent: red}",
            "bg: url(https://x/y.png)",
            "bg: image-set(\"a.png\")",
            "font: a/b",
            "accent: rgb(1,2,3",
            "accent: rgb 1,2,3)",
            "accent: \\65",
            "Accent: red",
            "--accent: red",
            "light: red",
            "size.foo: 1px",
            "accent",
            "accent:",
        ] {
            assert!(Theme::parse_lines(bad).is_err(), "accepted {bad:?}");
        }
        assert!(Theme::parse_lines(&format!("accent: {}", "a".repeat(200))).is_err());
        assert!(Theme::parse_lines(&"accent: red\n".repeat(100)).is_err());
    }

    #[test]
    fn allowed_functions_and_quoted_fonts_pass() {
        for ok in [
            "accent: color-mix(in srgb, #0a7a45 80%, white)",
            "measure: clamp(40rem, 90vw, 60rem)",
            "font: 'Helvetica Neue', Arial, sans-serif",
            "accent: var(--fg)",
            "size: 15px",
        ] {
            assert!(Theme::parse_lines(ok).is_ok(), "rejected {ok:?}");
        }
    }

    #[test]
    fn a_broken_or_missing_stored_theme_is_the_default() {
        assert!(Theme::from_config("{}").is_empty());
        assert!(Theme::from_config("not json").is_empty());
        assert!(Theme::from_config(r#"{"theme":"dark"}"#).is_empty());
        assert!(Theme::from_config(r#"{"theme":{"bg":"url(x)"}}"#).is_empty());
        assert!(Theme::from_config(r#"{"theme":{"bg":1}}"#).is_empty());
    }
}

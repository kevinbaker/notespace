//! Hacker News comment HTML to markdown.
//!
//! HN stores a tiny, fixed subset -- `<p>`, `<i>`, `<b>`, `<a>`, `<pre><code>` -- so this is a
//! scanner over that subset rather than a general HTML parser. Anything unrecognised has its
//! tags dropped and its text kept.

/// Characters that would otherwise be read as markup. `_` is absent because CommonMark does not
/// emphasise intraword underscores, and escaping it mangles every `snake_case` identifier; `>`
/// is absent because HN's quoting convention *should* become a blockquote.
const ESCAPE: &[char] = &['\\', '`', '*', '[', ']'];

pub fn to_markdown(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            push_text(&mut out, &html[start..i]);
            continue;
        }
        let Some(tag) = Tag::at(html, i) else {
            push_text(&mut out, "<");
            i += 1;
            continue;
        };
        match tag.name.as_str() {
            "p" if !tag.closing => out.push_str("\n\n"),
            "br" => out.push('\n'),
            "i" | "em" => out.push('*'),
            "b" | "strong" => out.push_str("**"),
            "pre" if !tag.closing => {
                i = code_block(html, tag.end, &mut out);
                continue;
            }
            "a" if !tag.closing => {
                i = link(html, &tag, &mut out);
                continue;
            }
            _ => {}
        }
        i = tag.end;
    }
    tidy(&out)
}

struct Tag {
    name: String,
    closing: bool,
    attrs: String,
    /// Byte offset just past `>`.
    end: usize,
}

impl Tag {
    fn at(html: &str, start: usize) -> Option<Tag> {
        let rest = &html[start + 1..];
        let close = rest.find('>')?;
        let inner = &rest[..close];
        let (closing, inner) = match inner.strip_prefix('/') {
            Some(s) => (true, s),
            None => (false, inner),
        };
        let inner = inner.trim_end_matches('/');
        let split = inner
            .find(|c: char| c.is_whitespace())
            .unwrap_or(inner.len());
        let name = inner[..split].to_ascii_lowercase();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        Some(Tag {
            name,
            closing,
            attrs: inner[split..].to_string(),
            end: start + 1 + close + 1,
        })
    }

    fn attr(&self, key: &str) -> Option<String> {
        let at = self.attrs.to_ascii_lowercase().find(&format!("{key}="))?;
        let after = &self.attrs[at + key.len() + 1..];
        let quote = after.chars().next()?;
        if quote == '"' || quote == '\'' {
            let end = after[1..].find(quote)?;
            Some(after[1..1 + end].to_string())
        } else {
            let end = after
                .find(|c: char| c.is_whitespace())
                .unwrap_or(after.len());
            Some(after[..end].to_string())
        }
    }
}

/// `<pre><code>…</code></pre>` becomes a fenced block. The body is entity-decoded but never
/// escaped: inside a fence, markdown has no syntax left to protect it from.
fn code_block(html: &str, from: usize, out: &mut String) -> usize {
    let body_start = match html[from..].find('>') {
        // Skip the nested <code> if there is one.
        Some(gt) if html[from..from + gt].trim_start().starts_with("<code") => from + gt + 1,
        _ => from,
    };
    let (body, end) = match html[body_start..].find("</pre>") {
        Some(at) => (&html[body_start..body_start + at], body_start + at + 6),
        None => (&html[body_start..], html.len()),
    };
    let body = decode_entities(strip_tags(body).trim_matches('\n'));
    // A body containing its own fence would end the block early.
    let fence = if body.contains("```") { "````" } else { "```" };
    out.push_str("\n\n");
    out.push_str(fence);
    out.push('\n');
    out.push_str(&body);
    out.push('\n');
    out.push_str(fence);
    out.push_str("\n\n");
    end
}

/// HN renders a link's href and its (often truncated) display text separately, and escapes
/// entities in both.
fn link(html: &str, tag: &Tag, out: &mut String) -> usize {
    let href = decode_entities(&tag.attr("href").unwrap_or_default());
    let (text, end) = match html[tag.end..].find("</a>") {
        Some(at) => (
            decode_entities(&strip_tags(&html[tag.end..tag.end + at])),
            tag.end + at + 4,
        ),
        None => (String::new(), tag.end),
    };
    if href.is_empty() {
        push_text(out, &text);
        return end;
    }
    // HN's display text is the href elided with an ellipsis; a link whose label repeats its
    // target is noise, so those become autolinks.
    let same = text.is_empty() || text == href || href.starts_with(text.trim_end_matches('.'));
    if same {
        out.push('<');
        out.push_str(&href);
        out.push('>');
    } else {
        out.push('[');
        push_text(out, &text);
        out.push_str("](");
        if href.contains(['(', ')', ' ']) {
            out.push('<');
            out.push_str(&href);
            out.push('>');
        } else {
            out.push_str(&href);
        }
        out.push(')');
    }
    end
}

fn push_text(out: &mut String, raw: &str) {
    for ch in decode_entities(raw).chars() {
        if ESCAPE.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for ch in s.chars() {
        match ch {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'&' {
                i += 1;
            }
            out.push_str(&s[start..i]);
            continue;
        }
        // A bare `&` is far more common than a malformed entity, so anything unterminated
        // within a plausible length is emitted as-is.
        //
        // Scanned as bytes and sliced only at `&` and `;`, both ASCII: the window is a fixed
        // byte count, so it can otherwise land inside a multi-byte character.
        let end = (i + 12).min(bytes.len());
        let mut semi = i + 1;
        while semi < end && bytes[semi] != b';' {
            semi += 1;
        }
        match (semi < end).then(|| named(&s[i + 1..semi])).flatten() {
            Some(ch) => {
                out.push(ch);
                i = semi + 1;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

fn named(body: &str) -> Option<char> {
    match body {
        "amp" => return Some('&'),
        "lt" => return Some('<'),
        "gt" => return Some('>'),
        "quot" => return Some('"'),
        "apos" => return Some('\''),
        "nbsp" => return Some(' '),
        _ => {}
    }
    let digits = body.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse().ok()?,
    };
    char::from_u32(code)
}

/// Collapse the blank lines `<p>` handling leaves behind, and drop trailing whitespace that
/// would otherwise become a hard line break.
fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0usize;
    for line in s.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blanks += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blanks > 0 { "\n\n" } else { "\n" });
        }
        blanks = 0;
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_become_blank_lines() {
        assert_eq!(to_markdown("one<p>two<p>three"), "one\n\ntwo\n\nthree");
    }

    #[test]
    fn italics_become_emphasis() {
        assert_eq!(
            to_markdown("read <i>The Dispossessed</i> first"),
            "read *The Dispossessed* first"
        );
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(
            to_markdown("it&#x27;s &quot;fine&quot; &amp; 1 &lt; 2"),
            "it's \"fine\" & 1 < 2"
        );
    }

    #[test]
    fn an_entity_window_landing_mid_character_does_not_panic() {
        // The 12-byte lookahead for `;` used to slice straight through a smart quote.
        assert_eq!(
            to_markdown("Tom & \u{201c}Jerry\u{201d} & co"),
            "Tom & \u{201c}Jerry\u{201d} & co"
        );
        assert_eq!(
            decode_entities("&\u{2014}\u{2014}\u{2014}\u{2014}&amp;"),
            "&\u{2014}\u{2014}\u{2014}\u{2014}&"
        );
    }

    #[test]
    fn a_bare_ampersand_survives() {
        assert_eq!(to_markdown("Tom & Jerry & co"), "Tom & Jerry & co");
    }

    #[test]
    fn hrefs_are_entity_decoded() {
        // HN escapes the slashes in the attribute itself, not just in the display text.
        let md =
            to_markdown(r#"<a href="https:&#x2F;&#x2F;example.org&#x2F;x" rel="nofollow">x</a>"#);
        assert_eq!(md, "[x](https://example.org/x)");
    }

    #[test]
    fn a_link_labelled_with_its_own_target_becomes_an_autolink() {
        let md = to_markdown(
            r#"<a href="https://example.org/very/long/path">https://example.org/very...</a>"#,
        );
        assert_eq!(md, "<https://example.org/very/long/path>");
    }

    #[test]
    fn parenthesised_urls_use_the_pointy_destination_form() {
        let md = to_markdown(r#"<a href="https://e.org/a_(b)">label</a>"#);
        assert_eq!(md, "[label](<https://e.org/a_(b)>)");
    }

    #[test]
    fn code_blocks_are_fenced_and_left_unescaped() {
        let md = to_markdown("see<p><pre><code>let x = *p;\n</code></pre>done");
        assert!(md.contains("```\nlet x = *p;\n```"), "got: {md}");
    }

    #[test]
    fn a_code_block_containing_a_fence_gets_a_longer_one() {
        let md = to_markdown("<pre><code>```\nnested\n```</code></pre>");
        assert!(md.starts_with("````"), "got: {md}");
        assert!(md.ends_with("````"), "got: {md}");
    }

    #[test]
    fn markup_characters_in_prose_are_escaped() {
        // Otherwise `*` pairs invent emphasis HN never rendered.
        assert_eq!(
            to_markdown("a *literal* star and [brackets]"),
            r"a \*literal\* star and \[brackets\]"
        );
    }

    #[test]
    fn underscores_are_left_alone() {
        assert_eq!(to_markdown("call foo_bar_baz()"), "call foo_bar_baz()");
    }

    #[test]
    fn hn_quoting_survives_as_a_blockquote() {
        assert_eq!(to_markdown("<p>&gt; quoted<p>reply"), "> quoted\n\nreply");
    }

    #[test]
    fn unknown_tags_lose_the_tag_and_keep_the_text() {
        assert_eq!(to_markdown("<span class=x>kept</span>"), "kept");
    }

    #[test]
    fn an_unclosed_angle_bracket_is_text() {
        assert_eq!(to_markdown("5 < 6 and 7"), "5 < 6 and 7");
    }

    #[test]
    fn output_has_no_leading_or_trailing_blank_lines() {
        let md = to_markdown("<p>first<p>last<p>");
        assert_eq!(md, "first\n\nlast");
    }

    #[test]
    fn round_trips_through_the_real_renderer() {
        let md = to_markdown("it&#x27;s <i>here</i><p>&gt; quoted");
        let html = notespace_render::markdown_to_html(&md);
        assert!(html.contains("<em>here</em>"), "got: {html}");
        assert!(html.contains("<blockquote>"), "got: {html}");
    }
}

//! RSS for a thread. Baked like the page: user-agnostic, cache-keyed by the thread's version.

use crate::time::civil_from_days;
use notespace_core::model::{PostState, ThreadPage};

/// RSS 2.0. Bodies go in `description` HTML-escaped, which is how readers expect them.
/// `base` is `https://host` with no trailing slash.
pub fn thread_rss(base: &str, page: &ThreadPage) -> String {
    let t = &page.thread;
    let mut out = String::with_capacity(4096);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<rss version=\"2.0\"><channel>");
    out.push_str("<title>");
    out.push_str(&escape(&t.title));
    out.push_str("</title><link>");
    out.push_str(&escape(&format!("{base}/t/{}", t.public_id)));
    out.push_str("</link><description>");
    out.push_str(&escape(&format!("{} in {}", t.title, page.space.name)));
    out.push_str("</description>");
    for post in &page.posts {
        // Tombstones carry no content and are not items.
        if post.state != PostState::Visible {
            continue;
        }
        let link = format!("{base}/p/{}", post.public_id);
        out.push_str("<item><title>");
        out.push_str(&escape(&post.author_name));
        out.push_str("</title><link>");
        out.push_str(&escape(&link));
        out.push_str("</link><guid isPermaLink=\"true\">");
        out.push_str(&escape(&link));
        out.push_str("</guid><author>");
        out.push_str(&escape(&post.author_name));
        out.push_str("</author><pubDate>");
        out.push_str(&rfc2822(post.created_at));
        out.push_str("</pubDate><description>");
        out.push_str(&escape(&post.body_html));
        out.push_str("</description></item>");
    }
    out.push_str("</channel></rss>\n");
    out
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// Unix seconds or milliseconds -- posts store whichever the importer had -- to RFC 2822 UTC.
fn rfc2822(ts: i64) -> String {
    let secs = if ts > 100_000_000_000 { ts / 1000 } else { ts };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    // 1970-01-01 was a Thursday.
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(mo - 1) as usize];
    format!("{weekday}, {d:02} {month} {y:04} {h:02}:{m:02}:{s:02} +0000")
}

#[cfg(test)]
mod tests {
    use super::*;
    use notespace_core::id::PublicId;
    use notespace_core::model::*;
    use notespace_core::path::Path;

    fn page(posts: Vec<Post>) -> ThreadPage {
        ThreadPage {
            space: Space {
                id: 1,
                path: "general/".into(),
                name: "General".into(),
                parent_id: None,
                ranking: Ranking::Bump,
                depth_cap: 8,
                config: "{}".into(),
            },
            thread: Thread {
                id: 42,
                public_id: PublicId::new(1_735_689_600_000, 1).unwrap(),
                space_id: 1,
                kind: ThreadKind::Discussion,
                title: "Tom & Jerry <3".into(),
                url: None,
                author_id: 1,
                author_name: "alice".into(),
                created_at: 1_700_000_000,
                bumped_at: 1_700_000_000,
                post_count: posts.len() as u32,
                state: ThreadState::Visible,
                cache_version: 0,
            },
            posts,
            next_cursor: None,
        }
    }

    fn post(id: i64, state: PostState, html: &str) -> Post {
        Post {
            id,
            public_id: PublicId::new(1_735_689_600_000 + id as u64, id as u32).unwrap(),
            thread_id: 42,
            parent_id: None,
            path: Path::root(id as u32).unwrap(),
            depth: 0,
            author_id: 1,
            author_name: "alice".into(),
            body_md: None,
            body_html: html.into(),
            created_at: 1_700_000_000,
            edited_at: None,
            score: 0.0,
            state,
        }
    }

    #[test]
    fn the_feed_escapes_titles_and_bodies_and_skips_tombstones() {
        let xml = thread_rss(
            "https://forum.example",
            &page(vec![
                post(1, PostState::Visible, "<p>a &amp; b</p>"),
                post(2, PostState::Deleted, "<p>secret</p>"),
            ]),
        );
        assert!(xml.starts_with("<?xml"));
        assert!(xml.contains("<title>Tom &amp; Jerry &lt;3</title>"));
        assert!(xml.contains("&lt;p&gt;a &amp;amp; b&lt;/p&gt;"));
        assert!(!xml.contains("secret"));
        assert_eq!(xml.matches("<item>").count(), 1);
        assert!(xml.contains("<link>https://forum.example/p/"));
    }

    #[test]
    fn dates_are_rfc2822_in_either_unit() {
        assert_eq!(rfc2822(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(rfc2822(1_700_000_000), "Tue, 14 Nov 2023 22:13:20 +0000");
        assert_eq!(
            rfc2822(1_700_000_000_000),
            "Tue, 14 Nov 2023 22:13:20 +0000"
        );
        assert_eq!(rfc2822(951_782_400), "Tue, 29 Feb 2000 00:00:00 +0000");
    }
}

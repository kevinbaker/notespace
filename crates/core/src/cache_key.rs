//! Cache keys. Here rather than in `crates/worker` because that crate is not in
//! `default-members`, so tests written there never run.

/// Built rather than taken from the URL, so junk query parameters cannot split the entry and
/// bypass the cache. Internal to the Cache API, which is why the version can appear here while
/// the public URL stays `/t/{id}`.
///
/// `None` for anything that is not a plain hostname.
pub fn thread_key(
    host: &str,
    canonical_id: &str,
    version: i64,
    after: Option<&str>,
) -> Option<String> {
    if !is_hostname(host) {
        return None;
    }
    Some(match after {
        Some(cursor) => format!("https://{host}/t/{canonical_id}/v{version}?after={cursor}"),
        None => format!("https://{host}/t/{canonical_id}/v{version}"),
    })
}

/// Strict because this is interpolated into a URL.
fn is_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b':')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_has_exactly_one_key() {
        assert_eq!(
            thread_key("dev.notespace.org", "06a1yabw03jnhej1", 0, None).as_deref(),
            Some("https://dev.notespace.org/t/06a1yabw03jnhej1/v0")
        );
    }

    #[test]
    fn the_cursor_is_part_of_the_key_and_nothing_else_is() {
        let first = thread_key("h", "abc", 0, None);
        let second = thread_key("h", "abc", 0, Some("000C"));
        assert_ne!(first, second);
        assert_eq!(second.as_deref(), Some("https://h/t/abc/v0?after=000C"));
    }

    #[test]
    fn junk_query_parameters_cannot_split_the_entry() {
        let plain = thread_key("h", "abc", 3, None);
        for _junk in ["?x=1", "?x=2", "?utm_source=whatever"] {
            assert_eq!(thread_key("h", "abc", 3, None), plain);
        }
    }

    #[test]
    fn bumping_the_version_changes_the_key() {
        let before = thread_key("h", "abc", 7, None);
        let after = thread_key("h", "abc", 8, None);
        assert_ne!(before, after);
    }

    #[test]
    fn a_host_that_is_not_a_hostname_is_refused() {
        for bad in [
            "",
            "host/../evil",
            "host/t/other",
            "host?x=1",
            "host#frag",
            "host with space",
            "hos\nt",
            "hos\tt",
        ] {
            assert_eq!(thread_key(bad, "abc", 0, None), None, "accepted {bad:?}");
        }
    }

    #[test]
    fn a_host_with_a_port_is_allowed_for_local_development() {
        assert_eq!(
            thread_key("127.0.0.1:8787", "abc", 0, None).as_deref(),
            Some("https://127.0.0.1:8787/t/abc/v0")
        );
    }
}

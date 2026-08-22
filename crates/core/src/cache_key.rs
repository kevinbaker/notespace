//! Cache keys for the baked read path.
//!
//! Pure string work, kept here rather than beside the Worker's cache calls so the tests run
//! under `cargo test` — `crates/worker` is not in `default-members`, so tests written there are
//! never executed.

/// Build the cache key for a thread page.
///
/// Derived from *only* what changes the bytes: the canonical thread id and the page cursor.
/// Every other query parameter is dropped, and that is load-bearing rather than tidiness —
/// keying on the raw URL would let `?x=1`, `?x=2`, … miss forever and drive the full page read
/// on every request, which is a few thousand requests to exhaust a day's row budget.
///
/// `host` must already be the canonical lowercase id's host. Returns `None` for anything that is
/// not a plain hostname, rather than interpolating it into a URL.
pub fn thread_key(host: &str, canonical_id: &str, after: Option<&str>) -> Option<String> {
    if !is_hostname(host) {
        return None;
    }
    Some(match after {
        Some(cursor) => format!("https://{host}/t/{canonical_id}?after={cursor}"),
        None => format!("https://{host}/t/{canonical_id}"),
    })
}

/// Hostname, optionally with a port. Deliberately strict: this value is interpolated into a URL,
/// and a `/`, `?` or `#` reaching that position would change which key is written.
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
            thread_key("dev.notespace.org", "06a1yabw03jnhej1", None).as_deref(),
            Some("https://dev.notespace.org/t/06a1yabw03jnhej1")
        );
    }

    #[test]
    fn the_cursor_is_part_of_the_key_and_nothing_else_is() {
        let first = thread_key("h", "abc", None);
        let second = thread_key("h", "abc", Some("000C"));
        assert_ne!(first, second);
        assert_eq!(second.as_deref(), Some("https://h/t/abc?after=000C"));
    }

    /// The reason the key is built rather than taken from the URL: junk parameters must not
    /// multiply entries, or an attacker bypasses the cache by appending a counter.
    #[test]
    fn junk_query_parameters_cannot_split_the_entry() {
        // Whatever the caller saw in the query string, only these two inputs reach the key.
        let plain = thread_key("h", "abc", None);
        for _junk in ["?x=1", "?x=2", "?utm_source=whatever"] {
            assert_eq!(thread_key("h", "abc", None), plain);
        }
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
            assert_eq!(thread_key(bad, "abc", None), None, "accepted {bad:?}");
        }
    }

    #[test]
    fn a_host_with_a_port_is_allowed_for_local_development() {
        assert_eq!(
            thread_key("127.0.0.1:8787", "abc", None).as_deref(),
            Some("https://127.0.0.1:8787/t/abc")
        );
    }
}

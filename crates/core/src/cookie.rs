//! Reading and writing cookies.
//!
//! Small enough to hand-roll, and hand-rolling avoids a dependency in the wasm bundle for what
//! is a `split(';')` in one direction and a `format!` in the other.

/// Session cookie. `__Host-` is not decoration: the prefix is refused by browsers unless the
/// cookie is `Secure`, has `Path=/`, and carries no `Domain` — which together mean a subdomain
/// cannot set or overwrite it. Without it, control of `anything.notespace.org` is enough to
/// plant a session cookie on the apex.
pub const SESSION: &str = "__Host-ns_session";

/// Binds a CSRF token before there is a session to bind it to.
pub const ANON: &str = "__Host-ns_anon";

/// Read one cookie's value from a raw `Cookie:` header.
///
/// Takes the header rather than a request type so both targets share it — and so the tests run
/// under `cargo test`, which they do not in `crates/worker`.
pub fn get(header: Option<&str>, name: &str) -> Option<String> {
    header?
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim().to_string())
}

/// A `Set-Cookie` value.
///
/// `HttpOnly` so script cannot read the session; `SameSite=Lax` rather than `Strict` so that
/// following a link into the forum does not appear logged out; `Secure` and `Path=/` because
/// `__Host-` requires them.
pub fn set(name: &str, value: &str, max_age_secs: i64) -> String {
    format!("{name}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={max_age_secs}")
}

/// Expire a cookie. Same attributes as [`set`], because a browser matches on them.
pub fn clear(name: &str) -> String {
    format!("{name}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
}

/// Whether `next` is a safe place to redirect to after login.
///
/// Only a local absolute path. Anything else is an open redirect, and a login page that
/// forwards to an attacker's host is a phishing primitive with the site's own domain on it.
/// `//evil.example` is the case a naive `starts_with('/')` misses: browsers read it as
/// protocol-relative and leave the site.
pub fn safe_next(next: &str) -> Option<&str> {
    let ok = next.starts_with('/')
        && !next.starts_with("//")
        && !next.starts_with("/\\")
        && !next.contains(['\r', '\n']);
    ok.then_some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_cookie_among_others() {
        let h = Some("a=1; __Host-ns_session=abc123; b=2");
        assert_eq!(get(h, SESSION).as_deref(), Some("abc123"));
        assert_eq!(get(h, "a").as_deref(), Some("1"));
        assert_eq!(get(h, "missing"), None);
        assert_eq!(get(None, SESSION), None);
    }

    #[test]
    fn a_prefix_is_not_a_match() {
        // `ns_session` must not satisfy a lookup for `__Host-ns_session`.
        assert_eq!(get(Some("ns_session=wrong"), SESSION), None);
    }

    #[test]
    fn set_and_clear_carry_the_attributes_the_host_prefix_requires() {
        let s = set(SESSION, "v", 60);
        for required in ["Path=/", "Secure", "HttpOnly", "SameSite=Lax"] {
            assert!(s.contains(required), "{required} missing from {s}");
        }
        assert!(!s.contains("Domain"), "__Host- forbids Domain");
        assert!(clear(SESSION).contains("Max-Age=0"));
    }

    #[test]
    fn open_redirects_are_refused() {
        assert_eq!(safe_next("/t/abc"), Some("/t/abc"));
        assert_eq!(safe_next("/"), Some("/"));
        for bad in [
            "//evil.example",  // protocol-relative: leaves the site
            "/\\evil.example", // some browsers normalise this to //
            "https://evil.example",
            "http://evil.example",
            "evil.example",
            "/ok\r\nSet-Cookie: x=1", // header injection
        ] {
            assert_eq!(safe_next(bad), None, "accepted {bad:?}");
        }
    }
}

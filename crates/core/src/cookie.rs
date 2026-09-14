//! Reading and writing cookies. Hand-rolled to keep a dependency out of the wasm bundle.

/// Session cookie. Browsers refuse the `__Host-` prefix unless the cookie is `Secure`, `Path=/`
/// and `Domain`-less, which together stop a subdomain setting or overwriting it.
pub const SESSION: &str = "__Host-ns_session";

/// Set alongside [`SESSION`] and readable by script: the one bit that says "there is a
/// session", so a shared page can decide whether to ask `/api/me` who it is. Carries nothing
/// else, and the server never reads it; the session cookie is `HttpOnly` and stays so.
pub const SIGNED_IN: &str = "ns_in";

/// Binds a CSRF token before there is a session to bind it to.
pub const ANON: &str = "__Host-ns_anon";

/// The sealed state and nonce of a sign-in through a provider, between redirect and callback.
pub const OAUTH: &str = "__Host-ns_oauth";

/// The sealed identity a provider vouched for, between the callback and the username step.
pub const PENDING: &str = "__Host-ns_pending";

/// Takes the raw header rather than a request type, so both targets share it and it is testable.
pub fn get(header: Option<&str>, name: &str) -> Option<String> {
    header?
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim().to_string())
}

/// `SameSite=Lax` rather than `Strict`, so following a link in does not appear logged out;
/// `Secure` and `Path=/` because `__Host-` requires them.
pub fn set(name: &str, value: &str, max_age_secs: i64) -> String {
    format!("{name}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={max_age_secs}")
}

/// Expire a cookie. Same attributes as [`set`], because a browser matches on them.
pub fn clear(name: &str) -> String {
    format!("{name}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
}

/// The session cookie and its script-visible marker, set together so they cannot disagree.
pub fn set_session(value: &str, max_age_secs: i64) -> [String; 2] {
    [
        set(SESSION, value, max_age_secs),
        format!("{SIGNED_IN}=1; Path=/; Secure; SameSite=Lax; Max-Age={max_age_secs}"),
    ]
}

/// Both cookies, expired.
pub fn clear_session() -> [String; 2] {
    [
        clear(SESSION),
        format!("{SIGNED_IN}=; Path=/; Secure; SameSite=Lax; Max-Age=0"),
    ]
}

/// Local absolute paths only; anything else is an open redirect. `//evil.example` is the case a
/// naive `starts_with('/')` misses, since browsers read it as protocol-relative.
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

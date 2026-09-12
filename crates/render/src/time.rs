//! Timestamps for people. Posts store unix time in whichever unit the importer had, seconds or
//! milliseconds; both render the same. Always UTC, since a baked page cannot know the reader's
//! zone -- the `datetime` attribute is there for a script to localise later.

use maud::{html, Markup};

/// `<time datetime="2025-01-01T00:00:00Z">2025-01-01 00:00</time>`.
pub fn stamp(ts: i64) -> Markup {
    let (y, mo, d, h, mi, s) = civil(ts);
    html! {
        time datetime={ (format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")) } {
            (format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}"))
        }
    }
}

/// Seconds or milliseconds, decided by magnitude: nothing on this site is dated before 1973.
fn civil(ts: i64) -> (i64, i64, i64, i64, i64, i64) {
    let secs = if ts > 100_000_000_000 { ts / 1000 } else { ts };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Howard Hinnant's days-to-civil, which needs no table of month lengths.
pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn either_unit_renders_the_same_utc_stamp() {
        let s = stamp(1_735_689_600).into_string();
        assert_eq!(
            s,
            r#"<time datetime="2025-01-01T00:00:00Z">2025-01-01 00:00</time>"#
        );
        assert_eq!(stamp(1_735_689_600_000).into_string(), s);
    }

    #[test]
    fn leap_days_and_the_epoch_are_right() {
        assert!(stamp(0).into_string().contains("1970-01-01 00:00"));
        assert!(stamp(951_782_400)
            .into_string()
            .contains("2000-02-29 00:00"));
    }
}

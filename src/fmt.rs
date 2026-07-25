//! Short human-readable renderings shared by the CLI and the TUI.

use chrono::{DateTime, Utc};

use crate::model::parse_ts;

/// How long ago a stored timestamp was, as `2h` / `3d` / `just now`.
pub fn age(timestamp: &str, now: DateTime<Utc>) -> String {
    match parse_ts(timestamp) {
        Some(at) => compact_duration((now - at).num_seconds()).unwrap_or_else(|| "just now".into()),
        None => "?".to_string(),
    }
}

/// How long is left on a lease, as `12m left` or `overdue`.
pub fn lease_remaining(expires_at: &str, now: DateTime<Utc>) -> String {
    match parse_ts(expires_at) {
        Some(at) => {
            let secs = (at - now).num_seconds();
            if secs <= 0 {
                "overdue".to_string()
            } else {
                format!(
                    "{} left",
                    compact_duration(secs).unwrap_or_else(|| "<1m".into())
                )
            }
        }
        None => "?".to_string(),
    }
}

/// `45s`, `12m`, `3h`, `2d`. `None` for anything under a second.
fn compact_duration(secs: i64) -> Option<String> {
    match secs {
        s if s < 1 => None,
        s if s < 60 => Some(format!("{s}s")),
        s if s < 3600 => Some(format!("{}m", s / 60)),
        s if s < 86_400 => Some(format!("{}h", s / 3600)),
        s => Some(format!("{}d", s / 86_400)),
    }
}

/// Collapse newlines and clamp to `width` graphemes-ish, with an ellipsis.
///
/// Character-based rather than grapheme-based, which is enough for the task
/// titles and assertion snippets this is used on.
pub fn truncate(text: &str, width: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= width {
        return flat.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let kept: String = flat.chars().take(width - 1).collect();
    format!("{}…", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::fmt_ts;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn ago(secs: i64) -> String {
        fmt_ts(now() - chrono::Duration::seconds(secs))
    }

    #[test]
    fn ages_use_the_largest_sensible_unit() {
        assert_eq!(age(&ago(0), now()), "just now");
        assert_eq!(age(&ago(45), now()), "45s");
        assert_eq!(age(&ago(90), now()), "1m");
        assert_eq!(age(&ago(7200), now()), "2h");
        assert_eq!(age(&ago(172_800), now()), "2d");
    }

    #[test]
    fn an_unparseable_timestamp_renders_as_a_question_mark() {
        assert_eq!(age("not a date", now()), "?");
        assert_eq!(lease_remaining("not a date", now()), "?");
    }

    #[test]
    fn leases_count_down_and_then_read_overdue() {
        let future = fmt_ts(now() + chrono::Duration::seconds(720));
        assert_eq!(lease_remaining(&future, now()), "12m left");
        assert_eq!(lease_remaining(&ago(1), now()), "overdue");
        assert_eq!(lease_remaining(&fmt_ts(now()), now()), "overdue");
    }

    #[test]
    fn truncation_adds_an_ellipsis_only_when_it_cuts() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly-ten", 11), "exactly-ten");
        assert_eq!(truncate("a much longer title", 10), "a much lo…");
        assert_eq!(truncate("abc", 1), "…");
    }

    #[test]
    fn truncation_flattens_newlines_and_trims() {
        assert_eq!(truncate("  two\nlines  ", 20), "two lines");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("ααααα", 5), "ααααα");
        assert_eq!(truncate("ααααα", 3), "αα…");
    }
}

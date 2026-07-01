//! Small display-string formatters shared by hrdr's frontends: compact token
//! counts and human-friendly relative times. Pure string builders — the caller
//! decides where/how to paint them.

/// Compact token count: `840`, `12.4k`, `1.8M`.
pub fn fmt_count(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Human-friendly elapsed time since `then`, with compound units for the larger
/// ranges (`now`, `42s ago`, `5m ago`, `1h30m ago`, `2d3h ago`).
pub fn relative_time(then: chrono::DateTime<chrono::Local>) -> String {
    let secs = (chrono::Local::now() - then).num_seconds().max(0);
    if secs < 5 {
        "now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        let (h, m) = (secs / 3600, (secs % 3600) / 60);
        if m > 0 {
            format!("{h}h{m}m ago")
        } else {
            format!("{h}h ago")
        }
    } else {
        let (d, h) = (secs / 86_400, (secs % 86_400) / 3600);
        if h > 0 {
            format!("{d}d{h}h ago")
        } else {
            format!("{d}d ago")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_count_ranges() {
        assert_eq!(fmt_count(840), "840");
        assert_eq!(fmt_count(12_400), "12.4k");
        assert_eq!(fmt_count(1_800_000), "1.8M");
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
    }

    #[test]
    fn relative_time_ranges() {
        use chrono::Duration;
        let now = chrono::Local::now();
        assert_eq!(relative_time(now), "now");
        assert_eq!(relative_time(now - Duration::seconds(42)), "42s ago");
        assert_eq!(relative_time(now - Duration::minutes(5)), "5m ago");
        assert_eq!(
            relative_time(now - Duration::minutes(90)),
            "1h30m ago" // 1h30m
        );
        assert_eq!(
            relative_time(now - Duration::hours(2) - Duration::minutes(1)),
            "2h1m ago"
        );
        assert_eq!(
            relative_time(now - Duration::days(2) - Duration::hours(3)),
            "2d3h ago"
        );
        // Future times clamp to "now" (negative elapsed).
        assert_eq!(relative_time(now + Duration::hours(1)), "now");
    }
}

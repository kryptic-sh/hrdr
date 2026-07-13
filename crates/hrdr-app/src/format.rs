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

/// Estimated-cost display: sub-cent totals keep four decimals so they don't
/// round to a lying "$0.00"; everything else shows cents ("$0.0042", "$1.24").
pub fn fmt_cost(usd: f64) -> String {
    if usd > 0.0 && usd < 0.01 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

/// Human-friendly elapsed time since `then`, with compound units for the larger
/// ranges (`now`, `42s ago`, `5m ago`, `1h30m ago`, `2d3h ago`).
pub fn relative_time(then: chrono::DateTime<chrono::Local>) -> String {
    relative_time_since(then, chrono::Local::now())
}

/// [`relative_time`] against a caller-supplied `now`.
///
/// A frame renders many of these, and reading the clock (with its timezone
/// lookup) per entry is pure waste: take the reading once and pass it in.
pub fn relative_time_since(
    then: chrono::DateTime<chrono::Local>,
    now: chrono::DateTime<chrono::Local>,
) -> String {
    let secs = (now - then).num_seconds().max(0);
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

/// The value [`relative_time_since`]'s output is a function of: two instants that
/// share a bucket render the same string, and a changed bucket means the string
/// changed.
///
/// It exists so a renderer can *key a cache* on a relative time without building
/// the string — a formatted timestamp per entry per frame is an allocation (and a
/// clock read) for a label that changes at most once a minute.
/// `bucket_agrees_with_the_string_it_stands_for` holds the two in step.
pub fn relative_time_bucket(
    then: chrono::DateTime<chrono::Local>,
    now: chrono::DateTime<chrono::Local>,
) -> u64 {
    let secs = (now - then).num_seconds().max(0) as u64;
    // Each arm mirrors a range of `relative_time_since`, reduced to the coarsest
    // value the string is sensitive to, and offset so ranges cannot collide.
    match secs {
        0..=4 => 0,                          // "now"
        5..=59 => 100 + secs,                // seconds
        60..=3599 => 1_000 + secs / 60,      // minutes
        3600..=86_399 => 10_000 + secs / 60, // hours + minutes
        _ => 100_000 + secs / 3600,          // days + hours
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
    fn fmt_cost_keeps_subcent_precision() {
        assert_eq!(fmt_cost(0.0042), "$0.0042");
        assert_eq!(fmt_cost(0.0), "$0.00");
        assert_eq!(fmt_cost(1.237), "$1.24");
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

    /// The bucket must be a faithful stand-in for the string: same bucket → same
    /// string, different bucket → different string.
    ///
    /// The renderer keys its per-block cache on the bucket precisely so it never
    /// has to build the string. If the bucket were coarser than the string, a
    /// timestamp would freeze on screen ("5m ago" long after it became "7m ago");
    /// if it were finer, every block would be re-laid-out on a clock tick and the
    /// cache would be worthless. Sweep the ranges and the boundaries between them.
    #[test]
    fn bucket_agrees_with_the_string_it_stands_for() {
        use chrono::Duration;
        let now = chrono::Local::now();
        let secs: Vec<i64> = (0..130)
            .chain([300, 359, 360, 3599, 3600, 3601, 3660, 7199, 7200])
            .chain([86_399, 86_400, 86_401, 90_000, 172_800, 176_400])
            .collect();

        let sample: Vec<(u64, String)> = secs
            .iter()
            .map(|s| {
                let then = now - Duration::seconds(*s);
                (
                    relative_time_bucket(then, now),
                    relative_time_since(then, now),
                )
            })
            .collect();

        for (i, (bucket_a, string_a)) in sample.iter().enumerate() {
            for (bucket_b, string_b) in &sample[i + 1..] {
                assert_eq!(
                    bucket_a == bucket_b,
                    string_a == string_b,
                    "bucket and string disagree: ({bucket_a}, {string_a}) vs ({bucket_b}, {string_b})"
                );
            }
        }
    }
}

/// The per-turn stats line both frontends append after a completed turn
/// (`✓ N tok · tok/s · elapsed · ttft · ctx`). `None` when the turn produced
/// nothing measurable. `ttft_secs` is time-to-first-token; the token rate is
/// measured from the first token, not the request start.
pub fn turn_stats_line(
    elapsed_secs: f64,
    ttft_secs: Option<f64>,
    out_tokens: usize,
    usage: Option<(u32, u32)>,
    cached_tokens: Option<u32>,
    reasoning_tokens: Option<u32>,
) -> Option<String> {
    if out_tokens == 0 && usage.is_none() {
        return None;
    }
    let speed = match ttft_secs {
        Some(t0) if out_tokens > 0 && elapsed_secs > t0 => out_tokens as f64 / (elapsed_secs - t0),
        _ => 0.0,
    };
    let mut s = format!("✓ {out_tokens} tok · {speed:.1} tok/s · {elapsed_secs:.1}s");
    // Time to first token (provider latency before streaming began).
    if let Some(t0) = ttft_secs {
        s.push_str(&format!(" · ttft {t0:.2}s"));
    }
    if let Some((prompt, completion)) = usage {
        let ratio = if completion > 0 {
            prompt as f64 / completion as f64
        } else {
            0.0
        };
        s.push_str(&format!(
            " · ctx {prompt} (in/out {prompt}/{completion}, {ratio:.1}:1)"
        ));
        // Prompt-cache hits + reasoning tokens, when the provider reports them.
        if let Some(c) = cached_tokens.filter(|c| *c > 0) {
            s.push_str(&format!(" · {c} cached"));
        }
        if let Some(r) = reasoning_tokens.filter(|r| *r > 0) {
            s.push_str(&format!(" · {r} reasoning"));
        }
    }
    Some(s)
}

/// Semantic role of one unified-diff line, for `/diff` coloring — the
/// classification both frontends map onto their theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// `+++`/`---` file headers and unclassified context: dim.
    Meta,
    /// `@@` hunk headers: user accent.
    Hunk,
    /// Added line: success green.
    Add,
    /// Removed line: error red.
    Remove,
}

/// Classify one line of a unified diff (see [`DiffLineKind`]).
pub fn classify_diff_line(line: &str) -> DiffLineKind {
    if line.starts_with("+++") || line.starts_with("---") {
        DiffLineKind::Meta
    } else if line.starts_with('@') {
        DiffLineKind::Hunk
    } else if line.starts_with('+') {
        DiffLineKind::Add
    } else if line.starts_with('-') {
        DiffLineKind::Remove
    } else {
        DiffLineKind::Meta
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;

    #[test]
    fn diff_line_classification() {
        use DiffLineKind::*;
        assert_eq!(classify_diff_line("+++ b/x.rs"), Meta);
        assert_eq!(classify_diff_line("--- a/x.rs"), Meta);
        assert_eq!(classify_diff_line("@@ -1,2 +1,3 @@"), Hunk);
        assert_eq!(classify_diff_line("+added"), Add);
        assert_eq!(classify_diff_line("-removed"), Remove);
        assert_eq!(classify_diff_line(" context"), Meta);
        assert_eq!(classify_diff_line("diff --git a/x b/x"), Meta);
    }

    #[test]
    fn turn_stats_line_shapes() {
        // Nothing measurable → no line.
        assert_eq!(turn_stats_line(1.0, None, 0, None, None, None), None);
        // Full line: rate measured from the first token, with cache + reasoning.
        let s =
            turn_stats_line(3.0, Some(1.0), 100, Some((600, 100)), Some(450), Some(30)).unwrap();
        assert!(s.contains("✓ 100 tok"), "{s}");
        assert!(s.contains("50.0 tok/s"), "{s}");
        assert!(s.contains("ttft 1.00s"), "{s}");
        assert!(s.contains("ctx 600 (in/out 600/100, 6.0:1)"), "{s}");
        assert!(s.contains("450 cached"), "{s}");
        assert!(s.contains("30 reasoning"), "{s}");
        // Usage-only turn (no streamed tokens) still reports context; zero
        // cache/reasoning are omitted.
        let s = turn_stats_line(2.0, None, 0, Some((10, 0)), Some(0), None).unwrap();
        assert!(s.contains("0.0 tok/s") && s.contains("ctx 10"), "{s}");
        assert!(!s.contains("cached"), "{s}");
    }
}

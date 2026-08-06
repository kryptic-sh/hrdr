//! Every duration constant says what unit it is in — checked, not trusted.
//!
//! The rule (documented on `DEFAULT_TOOL_TIMEOUT_SECS`):
//!
//! * a `Duration` constant carries **no** unit suffix, because the type does;
//! * a **scalar** constant must end in its unit — `_SECS`, `_MS`, or whatever
//!   non-time unit it counts (`_TURNS`, `_BYTES`, `_LINES`, `_TOKENS`).
//!
//! This exists because a bare `const SETTLE: u64 = 300` reads as seconds to
//! everyone who did not write it, and hrdr has already paid for that once: a
//! `timeout_ms` field renamed to seconds turned a test's `100` from a tenth of a
//! second into a minute and a half, and the suite went from 30 seconds to 500.
//! The unit belongs in the name, where a reader and a rename both trip over it.
//!
//! A doc comment saying "in milliseconds" is not this. It is not checked, it is
//! not visible at the call site, and it is exactly what was there before.

// Links the sandboxing constructor, so this binary cannot reach the developer's
// real $HOME (enforced by `every_test_binary_is_sandboxed`, which caught this
// file the first time it ran).
extern crate hrdr_test_support;

use std::path::{Path, PathBuf};

/// Words that mean "this constant is a span of time or a countdown".
const DURATION_WORDS: [&str; 7] = [
    "TIMEOUT", "INTERVAL", "DEBOUNCE", "POLL", "TTL", "DELAY", "BACKOFF",
];

/// Unit suffixes a scalar duration constant may end with. `_TURNS` is here
/// because hrdr counts one TTL in conversation turns rather than time — which is
/// precisely the case where the unit must be in the name.
const UNIT_SUFFIXES: [&str; 6] = ["_SECS", "_MS", "_TURNS", "_BYTES", "_LINES", "_TOKENS"];

#[test]
fn duration_constants_name_their_unit() {
    let mut offenders = Vec::new();
    let mut checked = 0usize;

    for file in rust_sources() {
        let text = std::fs::read_to_string(&file).expect("a readable source file");
        for (n, line) in text.lines().enumerate() {
            let Some((name, ty)) = const_decl(line) else {
                continue;
            };
            if !DURATION_WORDS.iter().any(|w| name.contains(w)) {
                continue;
            }
            // A duration is a number or a `Duration`. A `&str` that happens to
            // contain "POLL" is a message about polling, not a span of time —
            // `TASK_TOOL_POLL_MSG` is the one this caught on its first run.
            if !is_duration_type(ty) {
                continue;
            }
            checked += 1;
            if let Some(why) = violation(name, ty) {
                offenders.push(format!("{}:{}: {why}", file.display(), n + 1));
            }
        }
    }

    assert!(
        checked >= 20,
        "only {checked} duration constants were scanned — the parser probably \
         stopped matching, which would make this test pass by seeing nothing"
    );
    assert!(
        offenders.is_empty(),
        "duration constants must name their unit:\n  {}",
        offenders.join("\n  ")
    );
}

/// Why `NAME: ty` breaks the rule, or `None` when it holds.
fn violation(name: &str, ty: &str) -> Option<String> {
    let has_unit = UNIT_SUFFIXES.iter().any(|s| name.ends_with(s));
    match (ty.contains("Duration"), has_unit) {
        // A scalar with no unit in its name: the defect this test exists for.
        (false, false) => Some(format!(
            "`{name}: {ty}` is a bare number — end it with a unit ({}), or make it \
             a `Duration`",
            UNIT_SUFFIXES.join("/")
        )),
        // A `Duration` that also spells a unit says it twice, and the two can
        // disagree after an edit.
        (true, true) => Some(format!(
            "`{name}` is a `Duration`, so drop the unit suffix — the type already \
             carries it"
        )),
        _ => None,
    }
}

/// Whether `ty` can hold a span of time: a `Duration`, or a plain number. Types
/// that cannot (a `&str`, a struct) are out of scope however they are named.
fn is_duration_type(ty: &str) -> bool {
    ty.contains("Duration")
        || matches!(
            ty,
            "u8" | "u16" | "u32" | "u64" | "usize" | "i32" | "i64" | "f32" | "f64"
        )
}

/// `(NAME, type)` for a `const NAME: type = …` line, at any visibility and any
/// indentation (function-local constants count too). `None` for anything else.
fn const_decl(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim_start();
    let rest = rest.strip_prefix("pub(crate) ").unwrap_or(rest);
    let rest = rest.strip_prefix("pub ").unwrap_or(rest);
    let rest = rest.strip_prefix("const ")?;
    let (name, rest) = rest.split_once(':')?;
    let ty = rest.split('=').next()?.trim();
    let name = name.trim();
    // Generic consts and lifetimes are not what this is about.
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
        return None;
    }
    Some((name, ty))
}

/// Every `.rs` file under each workspace crate's `src/` and `tests/`.
fn rust_sources() -> Vec<PathBuf> {
    let root = hrdr_test_support::workspace_root();
    let mut out = Vec::new();
    for dir in ["crates", "apps"] {
        collect(&root.join(dir), &mut out);
    }
    assert!(!out.is_empty(), "the workspace has Rust sources");
    out
}

/// Recursively gather `.rs` files, skipping build output — `target/` holds
/// generated code from dependencies, which this rule does not govern.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The scan is only worth running if it can fail. These pin the two shapes it
/// rejects and the two it must not, against synthetic declarations — so the rule
/// is verified without waiting for someone to commit a violation.
#[test]
fn the_rule_catches_what_it_claims_to() {
    // Rejected: a bare scalar, and a `Duration` that repeats the unit.
    assert!(violation("NAV_TIMEOUT", "u64").is_some());
    assert!(violation("CACHE_TTL_SECS", "Duration").is_some());
    // Accepted: the two correct forms.
    assert!(violation("NAV_TIMEOUT_SECS", "u64").is_none());
    assert!(violation("SETTLE_DEBOUNCE_MS", "u64").is_none());
    assert!(violation("CACHE_TTL", "Duration").is_none());
    assert!(violation("DEFAULT_TODO_TTL_TURNS", "u64").is_none());

    // Types that cannot hold a duration are out of scope however they read.
    assert!(!is_duration_type("&str"));
    assert!(!is_duration_type("&'static str"));
    assert!(is_duration_type("u64"));
    assert!(is_duration_type("std::time::Duration"));

    // The line parser finds declarations at any visibility or indentation, and
    // ignores everything else.
    assert_eq!(
        const_decl("const A_TIMEOUT: u64 = 5;"),
        Some(("A_TIMEOUT", "u64"))
    );
    assert_eq!(
        const_decl("    pub(crate) const B_TTL: Duration = x;"),
        Some(("B_TTL", "Duration"))
    );
    assert_eq!(const_decl("let x: u64 = 5;"), None);
    assert_eq!(const_decl("// const C_TIMEOUT: u64 = 5;"), None);
}

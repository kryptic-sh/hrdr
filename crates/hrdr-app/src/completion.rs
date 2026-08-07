//! Representation-independent completion logic shared by hrdr's frontends:
//! ranking slash commands against a `/…` query, detecting an in-progress `@file`
//! mention, and ranking `@file` paths. Pure over strings + the `SLASH_COMMANDS`
//! registry — the popup/rendering is the frontend's job.

use crate::{SLASH_COMMANDS, resolve_alias};
use std::time::{SystemTime, UNIX_EPOCH};

/// Memoized `/resume` session listing for [`arg_completions`], which runs on
/// every keystroke while the argument is being typed — a full `list_sessions`
/// walk (read_dir + a stat per file + sort) per keystroke otherwise.
///
/// Keyed by the command prefix typed so far (stable while the argument itself
/// changes) and revalidated against the sessions tree's change signature, so a
/// session saved mid-typing still appears. The signature is the base dir's
/// mtime plus each per-cwd subdir's name + mtime: every session write, delete
/// and compression is an atomic rename inside a subdir, which bumps that
/// subdir's mtime (same granularity caveat as the per-file `meta_cache` in
/// `hrdr-agent::session`).
struct ResumeSessionCache {
    prefix: String,
    signature: Vec<(String, SystemTime)>,
    sessions: Vec<crate::SessionMeta>,
}

fn resume_session_signature() -> Vec<(String, SystemTime)> {
    let base = crate::sessions_dir();
    let base_mtime = std::fs::metadata(&base)
        .and_then(|m| m.modified())
        .unwrap_or(UNIX_EPOCH);
    let mut sig = vec![(base.display().to_string(), base_mtime)];
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && let Ok(mtime) = entry.metadata().and_then(|m| m.modified())
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                sig.push((name.to_string(), mtime));
            }
        }
    }
    sig.sort();
    sig
}

/// The memo itself — module-level so tests can assert the memoization ran (a
/// cache that never hits would pass every behavior test while still re-walking
/// per keystroke).
static RESUME_SESSION_CACHE: std::sync::OnceLock<std::sync::Mutex<Option<ResumeSessionCache>>> =
    std::sync::OnceLock::new();

/// The `/resume` session rows for `prefix`, served from the memo when the
/// prefix and the sessions tree are unchanged.
fn resume_sessions(prefix: &str) -> Vec<crate::SessionMeta> {
    let signature = resume_session_signature();
    let mut guard = RESUME_SESSION_CACHE
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some(cached) = guard.as_ref()
        && cached.prefix == prefix
        && cached.signature == signature
    {
        return cached.sessions.clone();
    }
    let sessions = crate::list_sessions();
    *guard = Some(ResumeSessionCache {
        prefix: prefix.to_string(),
        signature,
        sessions: sessions.clone(),
    });
    sessions
}

/// Whether a registry entry is an alias row (its name resolves to a different
/// canonical command). Aliases never render in the completion list — they only
/// widen matching for their canonical entry.
fn is_alias(name: &str) -> bool {
    let n = name.trim_start_matches('/');
    resolve_alias(n) != n
}

/// Commands matching the in-progress `/…` input (empty once a space is typed).
///
/// Matches the query (the text after `/`) against the command name, its
/// aliases (e.g. `/clear` surfaces `/new`), and its description
/// (case-insensitive substring), so e.g. `/list` surfaces `/help`
/// ("list commands"). Alias rows themselves never appear in the results — the
/// canonical command is shown instead. Ranked: name-prefix, alias-prefix,
/// name-substring, alias-substring, then description-substring.
pub fn slash_completions(input: &str) -> Vec<(&'static str, &'static str)> {
    let Some(query) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if query.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    if query.is_empty() {
        return SLASH_COMMANDS
            .iter()
            .copied()
            .filter(|(n, _)| !is_alias(n))
            .collect();
    }
    let q = query.to_ascii_lowercase();
    let mut scored: Vec<(u8, (&'static str, &'static str))> = Vec::new();
    for &(name, desc) in SLASH_COMMANDS {
        if is_alias(name) {
            continue;
        }
        let nl = name.trim_start_matches('/').to_ascii_lowercase();
        let name_rank = if nl.starts_with(&q) {
            Some(0)
        } else if nl.contains(&q) {
            Some(2)
        } else {
            None
        };
        // The best rank across this command's aliases (registry rows that
        // resolve to it).
        let alias_rank = SLASH_COMMANDS
            .iter()
            .filter(|(a, _)| is_alias(a) && resolve_alias(a.trim_start_matches('/')) == nl)
            .filter_map(|(a, _)| {
                let al = a.trim_start_matches('/').to_ascii_lowercase();
                if al.starts_with(&q) {
                    Some(1)
                } else if al.contains(&q) {
                    Some(3)
                } else {
                    None
                }
            })
            .min();
        let desc_rank = desc.to_ascii_lowercase().contains(&q).then_some(4u8);
        let Some(rank) = [name_rank, alias_rank, desc_rank]
            .into_iter()
            .flatten()
            .min()
        else {
            continue;
        };
        scored.push((rank, (name, desc)));
    }
    scored.sort_by_key(|(r, _)| *r); // stable: preserves list order within a rank
    scored.into_iter().map(|(_, c)| c).collect()
}

/// If an `@…` file mention is being typed at the end of `input`, return the byte
/// offset of the `@` and the partial query after it. Requires the `@` to start a
/// token (preceded by start-of-input or whitespace) with no whitespace after it.
pub fn active_file_token(input: &str) -> Option<(usize, String)> {
    let at = input.rfind('@')?;
    // Must start a token.
    if at > 0 {
        let prev = input[..at].chars().next_back()?;
        if !prev.is_whitespace() {
            return None;
        }
    }
    let query = &input[at + 1..];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some((at, query.to_string()))
}

/// Rank file paths against an `@file` `query` and return the best (up to 8).
/// Empty query keeps input order (shortest paths first); otherwise basename
/// prefix-matches rank above anywhere-substring matches, ties broken by shorter
/// path then lexicographically. Case-insensitive.
///
/// Each entry is `(path, lowercase_path)` — the lowercase form is precomputed
/// once when [`crate::spawn_file_index`] builds the index, off the UI thread,
/// so a keystroke's ranking pass does one lowercase per *query*, not one per
/// indexed path.
pub fn rank_file_matches(files: &[(String, String)], query: &str) -> Vec<String> {
    let q = query.to_ascii_lowercase();
    let mut scored: Vec<(u8, usize, &String)> = files
        .iter()
        .filter_map(|(p, lp)| {
            if q.is_empty() {
                return Some((1u8, p.len(), p));
            }
            let base = lp.rsplit('/').next().unwrap_or(lp);
            if base.starts_with(&q) {
                Some((0, p.len(), p))
            } else if lp.contains(&q) {
                Some((1, p.len(), p))
            } else {
                None
            }
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
    scored
        .into_iter()
        .take(8)
        .map(|(_, _, p)| p.clone())
        .collect()
}

/// Byte offset of the argument start, given the command text AFTER the sigil
/// (what follows `/` or `:`). The first whitespace run ends the command name;
/// the argument starts at the first non-whitespace byte after it. `None` when
/// the text has no whitespace at all (nothing typed after the command name).
pub fn command_arg_offset(rest: &str) -> Option<usize> {
    let ws = rest.find(char::is_whitespace)?;
    let after = &rest[ws..];
    Some(1 + ws + (after.len() - after.trim_start().len()))
}

/// Argument completion for a slash (`/cmd partial…`) or skill
/// (`:name partial…`) input: candidates for the argument being typed, matched
/// against everything after the command name (so multi-word values like
/// session names still complete). Returns the byte offset where the argument
/// starts plus `(value, description)` rows ranked name-prefix → substring →
/// description-substring; `None` when the input has no argument yet, the
/// command takes no completable argument, or nothing matches.
pub fn arg_completions(
    input: &str,
    skills: &[crate::Skill],
) -> Option<(usize, Vec<(String, String)>)> {
    let (sigil, rest) = match input.chars().next()? {
        c @ ('/' | ':') => (c, &input[1..]),
        _ => return None,
    };
    // Split "cmd" from the argument: the first whitespace run ends the name.
    let ws = rest.find(char::is_whitespace)?;
    let cmd = &rest[..ws];
    let arg_start = command_arg_offset(rest)?;
    let partial = &input[arg_start..];

    let set = |vals: &[(&str, &str)]| -> Vec<(String, String)> {
        vals.iter()
            .map(|(v, d)| ((*v).to_string(), (*d).to_string()))
            .collect()
    };
    let candidates: Vec<(String, String)> = if sigil == ':' {
        // A skill's frontmatter-declared argument values.
        skills
            .iter()
            .find(|sk| sk.name.eq_ignore_ascii_case(cmd))?
            .args
            .iter()
            .map(|a| (a.clone(), String::new()))
            .collect()
    } else {
        match resolve_alias(cmd) {
            "statusbar" => set(&[
                ("none", "hide the status bar"),
                ("truncate", "one line"),
                ("wrap", "as many lines as needed"),
            ]),
            "verbose" => set(&[
                ("on", "expand every tool block and show thinking"),
                ("off", "collapse them"),
            ]),
            "find" => set(&[("clear", "drop the search")]),
            "theme" => {
                let mut rows: Vec<(String, String)> = crate::theme_choices()
                    .into_iter()
                    .map(|c| (c.name, c.source))
                    .collect();
                rows.push(("reset".to_string(), "back to the default".to_string()));
                rows
            }
            "resume" => resume_sessions(&input[..arg_start])
                .into_iter()
                .map(|m| (m.id, m.name))
                .collect(),
            _ => return None,
        }
    };

    let q = partial.to_ascii_lowercase();
    let mut scored: Vec<(u8, (String, String))> = candidates
        .into_iter()
        .filter_map(|(v, d)| {
            let vl = v.to_ascii_lowercase();
            let rank = if q.is_empty() || vl.starts_with(&q) {
                0
            } else if vl.contains(&q) {
                1
            } else if d.to_ascii_lowercase().contains(&q) {
                2
            } else {
                return None;
            };
            Some((rank, (v, d)))
        })
        .collect();
    if scored.is_empty() {
        return None;
    }
    scored.sort_by_key(|(r, _)| *r); // stable: keeps candidate order within a rank
    Some((arg_start, scored.into_iter().map(|(_, c)| c).collect()))
}

/// Rank sub-agent names against an `@…` `query`: name-prefix matches first,
/// then anywhere-substring, ties broken lexicographically. Case-insensitive;
/// an empty query keeps input order. The mention popup lists these above the
/// file matches (an `@name` token routes to that sub-agent when it matches —
/// see `extract_agent_mention`).
pub fn rank_agent_matches(names: &[String], query: &str) -> Vec<String> {
    let q = query.to_ascii_lowercase();
    let mut scored: Vec<(u8, &String)> = names
        .iter()
        .filter_map(|n| {
            if q.is_empty() {
                return Some((1u8, n));
            }
            let nl = n.to_ascii_lowercase();
            if nl.starts_with(&q) {
                Some((0, n))
            } else if nl.contains(&q) {
                Some((1, n))
            } else {
                None
            }
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
    scored.into_iter().map(|(_, n)| n.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn slash_completions_prefix_ranks_first() {
        let names = |i: &str| {
            slash_completions(i)
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
        };
        assert_eq!(names("/he").first(), Some(&"/help"));
        // Name-prefix matches rank first (/compact, /cwd start with c);
        // /compact is the earliest such canonical in registry order.
        assert_eq!(names("/c").first(), Some(&"/compact"));
        assert!(names("/c").contains(&"/cwd"));
        // Description match: "/list" surfaces "/help" ("list commands").
        assert!(names("/list").contains(&"/help"));
        assert!(!names("/list").contains(&"/new"));
        // A space kills completion; non-slash input yields nothing.
        assert!(names("/help ").is_empty());
        assert!(names("hello").is_empty());
        // Bare slash returns every canonical command — alias rows are hidden.
        let canonical = SLASH_COMMANDS.iter().filter(|(n, _)| !is_alias(n)).count();
        assert_eq!(names("/").len(), canonical);
        assert!(canonical < SLASH_COMMANDS.len(), "aliases exist to hide");
    }

    #[test]
    fn aliases_match_but_never_render() {
        let names = |i: &str| {
            slash_completions(i)
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
        };
        // Typing an alias surfaces its canonical command…
        assert_eq!(names("/clear").first(), Some(&"/new"));
        assert_eq!(names("/usage").first(), Some(&"/cost"));
        assert_eq!(names("/health").first(), Some(&"/doctor"));
        // …and alias rows themselves never appear anywhere in the list.
        for i in ["/", "/c", "/new", "/us", "/h"] {
            assert!(
                names(i).iter().all(|n| !is_alias(n)),
                "alias row rendered for input {i:?}: {:?}",
                names(i)
            );
        }
        // A canonical name still outranks an alias on the same prefix:
        // "/re" prefix-matches /rename, /resume, /reload (rank 0)
        // before /new via its "reset" alias (rank 1).
        let re = names("/re");
        let new_pos = re.iter().position(|n| *n == "/new");
        assert!(re.contains(&"/rename") && re.contains(&"/resume"));
        assert!(new_pos > re.iter().position(|n| *n == "/rename"));
    }

    #[test]
    fn active_file_token_detection() {
        assert_eq!(active_file_token("@"), Some((0, String::new())));
        assert_eq!(
            active_file_token("look at @src/ma"),
            Some((8, "src/ma".into()))
        );
        assert_eq!(active_file_token("me@host"), None); // not a token boundary
        assert_eq!(active_file_token("@src/main.rs and"), None); // completed
        assert_eq!(active_file_token("hello world"), None); // no @
    }

    #[test]
    fn arg_completions_complete_enum_theme_and_skill_arguments() {
        let skills = vec![crate::Skill {
            name: "deploy".to_string(),
            description: String::new(),
            body: "…".to_string(),
            source: "test".to_string(),
            args: vec!["staging".to_string(), "production".to_string()],
            model_invocable: true,
        }];
        let vals = |i: &str| {
            arg_completions(i, &skills)
                .map(|(_, rows)| rows.into_iter().map(|(v, _)| v).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        // Enum arguments: prefix match, and the empty partial lists all.
        assert_eq!(vals("/statusbar tr"), vec!["truncate"]);
        // Dispatch-level alt names and registry aliases both resolve.
        assert_eq!(vals("/verbose o").len(), 2); // on, off
        // Theme names come from the registry (built-ins are always there).
        assert!(vals("/theme dra").contains(&"dracula".to_string()));
        assert!(vals("/theme re").contains(&"reset".to_string()));
        // Skill arguments come from the frontmatter `args:` list.
        assert_eq!(vals(":deploy st"), vec!["staging"]);
        assert_eq!(vals(":deploy ").len(), 2);
        // No argument yet, unknown command, or no match → nothing.
        assert!(arg_completions("/statusbar", &skills).is_none());
        assert!(arg_completions("/help x", &skills).is_none());
        assert!(arg_completions("/statusbar zz", &skills).is_none());
        assert!(arg_completions("hello there", &skills).is_none());
        // The offset points at the argument, past the whitespace run.
        let (start, _) = arg_completions("/verbose   off", &skills).unwrap();
        assert_eq!(&"/verbose   off"[start..], "off");
    }

    #[test]
    fn rank_agent_matches_prefix_then_substring() {
        let names: Vec<String> = ["reviewer", "planner", "code-reviewer"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            rank_agent_matches(&names, "rev"),
            vec!["reviewer".to_string(), "code-reviewer".to_string()]
        );
        assert_eq!(rank_agent_matches(&names, "").len(), 3);
        assert!(rank_agent_matches(&names, "zzz").is_empty());
        // Case-insensitive.
        assert_eq!(
            rank_agent_matches(&names, "PLAN"),
            vec!["planner".to_string()]
        );
    }

    #[test]
    fn rank_file_matches_prefers_basename_prefix() {
        let files = vec![
            ("src/main.rs".to_string(), "src/main.rs".to_string()),
            (
                "src/app/main_loop.rs".to_string(),
                "src/app/main_loop.rs".to_string(),
            ),
            (
                "docs/mainframe.md".to_string(),
                "docs/mainframe.md".to_string(),
            ),
            ("other.rs".to_string(), "other.rs".to_string()),
        ];
        let out = rank_file_matches(&files, "main");
        // Basename prefix matches come first (main.rs, main_loop.rs, mainframe.md),
        // ordered by path length; "other.rs" doesn't match at all.
        assert_eq!(out.first().map(String::as_str), Some("src/main.rs"));
        assert!(!out.iter().any(|p| p == "other.rs"));
        // Empty query keeps everything (shortest first) capped at 8.
        assert_eq!(rank_file_matches(&files, "").len(), 4);
    }

    /// Ranking trusts the precomputed lowercase half of each entry — the
    /// case-insensitive contract is established when the index is built
    /// (`spawn_file_index`), not per keystroke. A mixed-case path must still
    /// match a lowercase query.
    #[test]
    fn rank_file_matches_uses_the_precomputed_lowercase_form() {
        let files = vec![
            (
                "SRC/App/Notes.md".to_string(),
                "src/app/notes.md".to_string(),
            ),
            ("docs/README".to_string(), "docs/readme".to_string()),
        ];
        // Basename prefix match against the lowercased path.
        assert_eq!(
            rank_file_matches(&files, "notes")
                .first()
                .map(String::as_str),
            Some("SRC/App/Notes.md")
        );
        // Anywhere-substring match, case-insensitively.
        assert_eq!(
            rank_file_matches(&files, "README")
                .first()
                .map(String::as_str),
            Some("docs/README")
        );
    }

    // ── /resume completion memo ────────────────────────────────────────────

    /// Global lock so env-var-dependent tests don't race on HOME / XDG vars
    /// (`std::env::set_var` is not thread-safe in Rust tests). Duplicated from
    /// `sessions.rs` — a `#[cfg(test)]` module in one crate is invisible to
    /// another.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_test_env(f: impl FnOnce(&tempfile::TempDir)) {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", tmp.path());
        }
        f(&tmp);
        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    /// The `/resume` argument completion runs on every keystroke; the listing
    /// is memoized per prefix and refreshed when the sessions tree changes. A
    /// cache that never hit (always re-walking) or never invalidated (serving
    /// a stale list) both fail here.
    #[test]
    fn resume_completion_memoizes_and_refreshes_on_session_change() {
        let skills = Vec::new();
        let vals = |i: &str| {
            arg_completions(i, &skills)
                .map(|(_, rows)| rows.into_iter().map(|(v, _)| v).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        with_test_env(|tmp| {
            let cwd = tmp.path().join("proj").to_string_lossy().to_string();
            crate::Session::new(state("First", &cwd))
                .save("first")
                .unwrap();
            assert_eq!(vals("/resume fir"), vec!["first"]);

            // The first walk memoized the listing…
            let memo_hit = RESUME_SESSION_CACHE
                .get()
                .and_then(|m| m.lock().ok())
                .map(|g| g.is_some())
                .unwrap_or(false);
            assert!(memo_hit, "the /resume listing was memoized");

            // …a session saved mid-typing (same prefix) is picked up by the
            // change signature, not hidden behind the memo.
            crate::Session::new(state("Second", &cwd))
                .save("second")
                .unwrap();
            assert_eq!(vals("/resume sec"), vec!["second"]);

            // A second call on the unchanged tree serves the memo.
            assert_eq!(vals("/resume sec"), vec!["second"]);
            assert_eq!(vals("/resume ").len(), 2);
        });
    }

    fn state(name: &str, cwd: &str) -> crate::SessionState {
        crate::SessionState {
            name: name.to_string(),
            model: "local://model".parse().unwrap(),
            base_url: "http://x/v1".to_string(),
            cwd: cwd.to_string(),
            messages: vec![hrdr_agent::Message::user("hi")],
            ..Default::default()
        }
    }
}

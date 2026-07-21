//! Custom skills: reusable prompt templates invoked with a `:` prefix
//! (`:name args…`), shared by hrdr's frontends.
//!
//! A skill is a Markdown file — optional YAML frontmatter (`name:`,
//! `description:`, `args:`), body = the prompt. On invocation the body is sent
//! to the model with `$ARGUMENTS` filled from the text after the skill name: a
//! skill that declares `args:` takes just the first token as its argument and
//! appends any trailing text as extra context, while a skill without `args:`
//! takes the whole remainder (see [`expand_skill`]). Discovery mirrors the
//! sub-agent files: project dirs first, then
//! user dirs, hrdr → Claude Code → opencode conventions, then hrdr's own
//! built-in skills (`:commit`, `:release`, `:review`, `:audit`, `:fix`, `:todo`, `:test`, `:plan`) last — deduped by name
//! (first source wins), so a user or project file always overrides a
//! built-in of the same name.

use std::path::{Path, PathBuf};

// The skills hrdr ships with, baked into the binary via `include_str!` — the
// same convention `hrdr_agent::prompt` uses for `system.j2` — so a fresh
// install has a working `:commit`, `:release`, `:review`, `:audit`, `:fix`, `:todo`, `:test`, `:plan` with no setup.
// Content lives in `templates/skills/*.md`, not here: keep the prompt text in
// Markdown (reviewable, diffable, editable without touching Rust) and this
// file to parsing/wiring only.
const BUILTIN_COMMIT: &str = include_str!("templates/skills/commit.md");
const BUILTIN_RELEASE: &str = include_str!("templates/skills/release.md");
const BUILTIN_REVIEW: &str = include_str!("templates/skills/review.md");
const BUILTIN_AUDIT: &str = include_str!("templates/skills/audit.md");
const BUILTIN_TODO: &str = include_str!("templates/skills/todo.md");
const BUILTIN_TEST: &str = include_str!("templates/skills/test.md");
const BUILTIN_FIX: &str = include_str!("templates/skills/fix.md");
const BUILTIN_PLAN: &str = include_str!("templates/skills/plan.md");

/// Max bytes for a single skill file; files larger than this are skipped.
const MAX_SKILL_FILE_BYTES: u64 = 64 * 1024; // 64 KiB

/// Aggregate ceilings on skill ingestion across ALL skill dirs combined: at
/// most this many skill files read, and at most this many total bytes. A real
/// setup has a handful of small skill Markdown files, so 256 files / 4 MiB is
/// far beyond anything genuine — the cap only stops a hostile or accidental
/// directory full of files from making hrdr read unbounded bytes on every `:`
/// input and skill listing. Once either is hit we stop reading and warn; the
/// built-ins are always appended regardless.
const MAX_SKILLS: usize = 256;
const MAX_SKILLS_TOTAL_BYTES: usize = 4 * 1024 * 1024; // 4 MiB

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Invocation name (`:name`) — frontmatter `name:`, else the file stem.
    pub name: String,
    /// One-line summary for the completion popup / `/skills` listing.
    pub description: String,
    /// The prompt template (the file body).
    pub body: String,
    /// Where it came from, for the `/skills` listing (home-shortened dir).
    pub source: String,
    /// Candidate argument values (frontmatter `args:`, comma-separated or
    /// `[a, b]`), offered by the completion popup after `:name `.
    pub args: Vec<String>,
}

/// The skill directories to scan, in precedence order (highest first).
fn skill_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Project scopes (nearest / most specific) first.
    dirs.push(cwd.join(".hrdr").join("skills"));
    dirs.push(cwd.join(".claude").join("commands"));
    dirs.push(cwd.join(".opencode").join("command"));
    // User scopes.
    if let Some(d) = hrdr_agent::config_dir() {
        dirs.push(d.join("skills")); // ~/.config/hrdr/skills
    }
    if let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    {
        dirs.push(home.join(".claude").join("commands"));
    }
    if let Ok(d) = hjkl_xdg::config_dir("opencode") {
        dirs.push(d.join("command")); // ~/.config/opencode/command
    }
    dirs
}

/// Discover skill files across the hrdr/Claude/opencode locations, relative to
/// `cwd` for project scopes, plus hrdr's built-in skills. One skill per unique
/// name (case-insensitive); the first source in precedence order wins — the
/// built-ins are appended last, so any user or project file of the same name
/// (e.g. a project's own `.hrdr/skills/commit.md`) is discovered first and
/// shadows it.
pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    // Aggregate budget across ALL skill dirs combined. Dirs are scanned in
    // precedence order (project before user), so exhausting the budget drops
    // the least-specific files first.
    let mut file_count: usize = 0;
    let mut total_bytes: usize = 0;
    let mut truncated = false;
    for dir in skill_dirs(cwd) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<Skill> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if path.metadata().map(|m| m.len()).unwrap_or(0) > MAX_SKILL_FILE_BYTES {
                continue;
            }
            if file_count >= MAX_SKILLS || total_bytes >= MAX_SKILLS_TOTAL_BYTES {
                truncated = true;
                break;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            file_count += 1;
            total_bytes = total_bytes.saturating_add(text.len());
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(skill) = parse_skill_file(&text, stem, &crate::display_dir(&dir)) {
                found.push(skill);
            }
        }
        // Stable order within a directory (read_dir order is unspecified).
        found.sort_by(|a, b| a.name.cmp(&b.name));
        for skill in found {
            if !out.iter().any(|s| s.name.eq_ignore_ascii_case(&skill.name)) {
                out.push(skill);
            }
        }
        // Merge this dir's finds before stopping, so nothing already read is lost.
        if truncated {
            // Silent on purpose: `discover_skills` runs inside the TUI (on every
            // cwd change and `:`-completion), so writing to stderr here would
            // corrupt the display. The cap is a defensive ceiling (256 files /
            // 4 MiB) no real setup reaches, so there is nothing actionable to say.
            break;
        }
    }
    for skill in builtin_skills() {
        if !out.iter().any(|s| s.name.eq_ignore_ascii_case(&skill.name)) {
            out.push(skill);
        }
    }
    out
}

/// hrdr's built-in skills — `:commit`, `:release`, `:review`, `:audit`, `:fix`, `:todo`, `:test`, `:plan` — parsed from the
/// Markdown templates baked into the binary at compile time. Always eight
/// entries (each template is a checked-in, non-empty file, so parsing cannot
/// fail); sorted by name like a scanned directory's entries are, so their
/// relative order matches wherever they'd sit if they were plain files on
/// disk.
pub fn builtin_skills() -> Vec<Skill> {
    let mut skills: Vec<Skill> = [
        (BUILTIN_COMMIT, "commit"),
        (BUILTIN_RELEASE, "release"),
        (BUILTIN_REVIEW, "review"),
        (BUILTIN_AUDIT, "audit"),
        (BUILTIN_TODO, "todo"),
        (BUILTIN_TEST, "test"),
        (BUILTIN_FIX, "fix"),
        (BUILTIN_PLAN, "plan"),
    ]
    .into_iter()
    .filter_map(|(text, stem)| parse_skill_file(text, stem, "built-in"))
    .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Parse one skill file: optional YAML frontmatter (a leading `---` … `---`
/// fence containing `name:` / `description:` / `args:`), body = the prompt.
/// `None` when the body is empty.
pub fn parse_skill_file(text: &str, filename_stem: &str, source: &str) -> Option<Skill> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (name, description, args, body) = match hrdr_agent::split_fence(text) {
        Some((fm, body)) => {
            let (name, description, args) = parse_frontmatter(fm);
            (name, description, args, body)
        }
        None => (None, None, Vec::new(), text),
    };
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    Some(Skill {
        name: name.unwrap_or_else(|| filename_stem.to_string()),
        description: description.unwrap_or_default(),
        body: body.to_string(),
        source: source.to_string(),
        args,
    })
}

/// Extract `(name, description, args)` from a fence's frontmatter text via
/// real YAML parsing (`serde_yaml_ng`), rather than the old line-by-line
/// `key: value` scan — which silently dropped anything YAML-legal but not on
/// a single line: prettier wraps a long `description:` onto a continuation
/// line, and block scalars (`description: >` / `|`) or list-form `args:`
/// (`args:\n  - low\n  - high`) never matched at all.
///
/// Malformed YAML (not parseable, or not a mapping — e.g. the frontmatter is
/// a bare scalar or list) degrades gracefully to "no frontmatter" instead of
/// failing the whole skill: `split_fence` has already stripped the fence off
/// the body, so the raw frontmatter text never leaks into the prompt either
/// way, and the caller falls back to a stem-derived name with empty
/// description/args.
fn parse_frontmatter(fm: &str) -> (Option<String>, Option<String>, Vec<String>) {
    let Ok(serde_yaml_ng::Value::Mapping(map)) = serde_yaml_ng::from_str(fm) else {
        return (None, None, Vec::new());
    };
    let scalar = |key: &str| -> Option<String> {
        map.get(key)
            .and_then(scalar_to_string)
            .filter(|v| !v.is_empty())
    };
    let name = scalar("name");
    let description = scalar("description");
    // `args: [staging, production]` (already a YAML sequence) or list form
    // (`args:\n  - low\n  - high`) — stringify each element. A bare string
    // (`args: staging, production`) instead splits on commas, matching the
    // old flat-parser's comma-separated form.
    let args = match map.get("args") {
        Some(serde_yaml_ng::Value::Sequence(seq)) => seq
            .iter()
            .filter_map(scalar_to_string)
            .filter(|v| !v.is_empty())
            .collect(),
        Some(v) => scalar_to_string(v)
            .map(|s| {
                s.split(',')
                    .map(|a| a.trim().to_string())
                    .filter(|a| !a.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };
    (name, description, args)
}

/// Stringify a YAML scalar (string/number/bool), trimmed. `None` for `Null`
/// or a non-scalar (sequence/mapping/tagged) — those aren't valid values for
/// `name`/`description`/a single `args` element.
fn scalar_to_string(v: &serde_yaml_ng::Value) -> Option<String> {
    match v {
        serde_yaml_ng::Value::String(s) => Some(s.trim().to_string()),
        serde_yaml_ng::Value::Number(n) => Some(n.to_string()),
        serde_yaml_ng::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// If `input` invokes a skill (`:name args…`, matched case-insensitively),
/// return the prompt to send. `None` when the input isn't a `:` invocation or
/// names no known skill (it then goes to the model as-is).
///
/// How the text after the name is used depends on whether the skill declares
/// `args:`:
/// - A skill **with** `args:` takes a single positional argument — the first
///   whitespace-delimited token. That token fills `$ARGUMENTS`, and anything
///   after it is extra free-form context appended to the body on its own line.
///   So `:audit high focus on the parser` runs the audit at depth `high` with
///   "focus on the parser" appended as guidance.
/// - A skill **without** `args:` treats the whole remainder as `$ARGUMENTS`
///   (or, when the body has no placeholder, appends it) — free-form input like
///   a pasted error or a commit scope isn't split on the first space.
pub fn expand_skill(input: &str, skills: &[Skill]) -> Option<String> {
    let rest = input.trim_start().strip_prefix(':')?;
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next().filter(|n| !n.is_empty())?;
    let after_name = parts.next().unwrap_or("").trim();
    let skill = skills.iter().find(|s| s.name.eq_ignore_ascii_case(name))?;

    // A declared-`args:` skill consumes only its first token as the argument;
    // the rest is appended. A skill without `args:` takes the whole remainder.
    let (arg, extra) = if skill.args.is_empty() {
        (after_name, "")
    } else {
        let mut split = after_name.splitn(2, char::is_whitespace);
        (
            split.next().unwrap_or(""),
            split.next().unwrap_or("").trim(),
        )
    };

    let mut prompt = if skill.body.contains("$ARGUMENTS") {
        skill.body.replace("$ARGUMENTS", arg)
    } else if arg.is_empty() {
        skill.body.clone()
    } else {
        format!("{}\n\n{arg}", skill.body)
    };
    if !extra.is_empty() {
        prompt = format!("{prompt}\n\n{extra}");
    }
    Some(prompt)
}

/// Case-insensitive fuzzy filter over skills for the `/skills` picker: the
/// query's characters must appear in order within `"name description source"`.
/// Returns matching indices in input order; an empty query matches everything.
pub fn filter_skills(skills: &[Skill], query: &str) -> Vec<usize> {
    let q: Vec<char> = query.trim().to_lowercase().chars().collect();
    if q.is_empty() {
        return (0..skills.len()).collect();
    }
    skills
        .iter()
        .enumerate()
        .filter_map(|(i, sk)| {
            let hay = format!("{} {} {}", sk.name, sk.description, sk.source).to_lowercase();
            crate::is_subsequence(&q, &hay).then_some(i)
        })
        .collect()
}

/// Skills matching an in-progress `:…` input (empty once a space is typed) as
/// `(":name", description)` rows for the completion popup. Ranked like the
/// slash commands: name-prefix, then name-substring, then description.
pub fn skill_completions(input: &str, skills: &[Skill]) -> Vec<(String, String)> {
    let Some(query) = input.strip_prefix(':') else {
        return Vec::new();
    };
    if query.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let q = query.to_ascii_lowercase();
    let mut scored: Vec<(u8, (String, String))> = Vec::new();
    for s in skills {
        let nl = s.name.to_ascii_lowercase();
        let rank = if q.is_empty() || nl.starts_with(&q) {
            0
        } else if nl.contains(&q) {
            1
        } else if s.description.to_ascii_lowercase().contains(&q) {
            2
        } else {
            continue;
        };
        scored.push((rank, (format!(":{}", s.name), s.description.clone())));
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.0.cmp(&b.1.0)));
    scored.into_iter().map(|(_, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, desc: &str, body: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            body: body.to_string(),
            source: "test".to_string(),
            args: Vec::new(),
        }
    }

    #[test]
    fn parse_reads_frontmatter_and_falls_back_to_the_stem() {
        let s = parse_skill_file(
            "---\nname: ship\ndescription: release checklist\n---\nDo the release.",
            "stem",
            "src",
        )
        .unwrap();
        assert_eq!(s.name, "ship");
        assert_eq!(s.description, "release checklist");
        assert_eq!(s.body, "Do the release.");

        // No frontmatter: the stem names it, the whole text is the body.
        let s = parse_skill_file("Just a prompt.", "quick", "src").unwrap();
        assert_eq!(s.name, "quick");
        assert_eq!(s.body, "Just a prompt.");

        // Empty body → not a skill.
        assert!(parse_skill_file("---\nname: x\n---\n  \n", "x", "src").is_none());

        // `args:` declares completion candidates (bracketed or bare list).
        let s = parse_skill_file(
            "---\nargs: [staging, production]\n---\nDeploy $ARGUMENTS",
            "deploy",
            "src",
        )
        .unwrap();
        assert_eq!(s.args, vec!["staging", "production"]);
    }

    /// Regression test for the bug this module was rewritten to fix: prettier
    /// wraps a `description:` past 80 cols onto a plain continuation line,
    /// which is still valid YAML (folded into one space-joined string) but
    /// was invisible to the old line-by-line `key: value` scan.
    #[test]
    fn plain_continuation_scalar_description_is_not_lost() {
        let s = parse_skill_file(
            "---\nname: commit\ndescription:\n  stage and commit the working changes with a Conventional Commit message\n---\nDo it.",
            "commit",
            "src",
        )
        .unwrap();
        assert_eq!(
            s.description,
            "stage and commit the working changes with a Conventional Commit message"
        );
    }

    /// Block scalars — folded (`>`) and literal (`|`) — are real YAML that the
    /// old flat parser never understood; `serde_yaml_ng` handles them for
    /// free.
    #[test]
    fn block_scalar_descriptions_parse() {
        let folded = parse_skill_file(
            "---\ndescription: >\n  line one\n  line two\n---\nBody.",
            "stem",
            "src",
        )
        .unwrap();
        assert_eq!(folded.description, "line one line two");

        let literal = parse_skill_file(
            "---\ndescription: |\n  line one\n  line two\n---\nBody.",
            "stem",
            "src",
        )
        .unwrap();
        assert_eq!(literal.description, "line one\nline two");
    }

    /// `args:` as a YAML list (block sequence), the natural way to write
    /// multiple candidates across lines — distinct from the inline
    /// `[a, b]` / comma-string forms already covered elsewhere.
    #[test]
    fn args_as_yaml_list_parses() {
        let s = parse_skill_file(
            "---\nargs:\n  - low\n  - high\n---\nReview $ARGUMENTS",
            "review",
            "src",
        )
        .unwrap();
        assert_eq!(s.args, vec!["low", "high"]);
    }

    /// `args: staging, production` (bare comma string, no brackets) still
    /// splits into candidates — compat with the old flat parser's form.
    #[test]
    fn args_as_comma_string_parses() {
        let s = parse_skill_file(
            "---\nargs: staging, production\n---\nDeploy $ARGUMENTS",
            "deploy",
            "src",
        )
        .unwrap();
        assert_eq!(s.args, vec!["staging", "production"]);
    }

    /// Frontmatter that isn't valid YAML (a tab-indented line — tabs are
    /// illegal for YAML indentation) degrades gracefully: the skill still
    /// loads with a stem-derived name and the body intact, and — crucially —
    /// none of the raw frontmatter text leaks into the body sent to the
    /// model.
    #[test]
    fn invalid_yaml_frontmatter_degrades_without_leaking_into_body() {
        let s = parse_skill_file(
            "---\nname: x\n\tbad: tab-indented\n---\nDo the thing.",
            "stem",
            "src",
        )
        .unwrap();
        assert_eq!(s.name, "stem");
        assert_eq!(s.description, "");
        assert!(s.args.is_empty());
        assert_eq!(s.body, "Do the thing.");
        assert!(!s.body.contains("bad"));
        assert!(!s.body.contains("---"));
    }

    /// `description: has: colons` — an unquoted value containing `: ` is
    /// ambiguous plain-scalar syntax that YAML rejects as a parse error, not
    /// silently misparsed. Degrades the same way as any other invalid YAML.
    #[test]
    fn unquoted_colon_in_value_degrades_gracefully() {
        let s = parse_skill_file(
            "---\nname: x\ndescription: has: colons\n---\nBody text.",
            "stem",
            "src",
        )
        .unwrap();
        assert_eq!(s.name, "stem");
        assert_eq!(s.description, "");
        assert_eq!(s.body, "Body text.");
        assert!(!s.body.contains("colons"));
    }

    /// Frontmatter that parses as YAML but not as a mapping (e.g. a value
    /// containing an unquoted colon that YAML reads as a nested-mapping-like
    /// scalar ambiguity) also degrades gracefully rather than panicking or
    /// misparsing a field.
    #[test]
    fn non_mapping_frontmatter_degrades_gracefully() {
        let s =
            parse_skill_file("---\njust a plain string\n---\nBody text.", "stem", "src").unwrap();
        assert_eq!(s.name, "stem");
        assert_eq!(s.description, "");
        assert!(s.args.is_empty());
        assert_eq!(s.body, "Body text.");
    }

    /// Security regression: a CRLF-authored skill file (`---\r\n`) must still
    /// have its frontmatter parsed rather than falling through to "no fence",
    /// which would make the raw YAML (`name:`, `description:`, …) part of the
    /// prompt body sent to the model — covered by `hrdr_agent::split_fence`'s
    /// own CRLF handling, shared with `agents_dir.rs`'s `split_frontmatter`.
    #[test]
    fn crlf_frontmatter_is_still_parsed() {
        let s = parse_skill_file(
            "---\r\nname: ship\r\ndescription: release checklist\r\n---\r\nDo the release.\r\n",
            "stem",
            "src",
        )
        .unwrap();
        assert_eq!(s.name, "ship");
        assert_eq!(s.description, "release checklist");
        assert_eq!(s.body, "Do the release.");
    }

    #[test]
    fn expand_substitutes_arguments_or_appends() {
        let skills = vec![
            skill("review", "", "Review the diff.\nFocus: $ARGUMENTS"),
            skill("ship", "", "Run the release checklist."),
        ];
        // $ARGUMENTS placeholder is substituted (matched case-insensitively).
        assert_eq!(
            expand_skill(":Review error handling", &skills).unwrap(),
            "Review the diff.\nFocus: error handling"
        );
        // No placeholder: args append on their own line…
        assert_eq!(
            expand_skill(":ship v2 only", &skills).unwrap(),
            "Run the release checklist.\n\nv2 only"
        );
        // …and no args leaves the body untouched.
        assert_eq!(
            expand_skill(":ship", &skills).unwrap(),
            "Run the release checklist."
        );
        // Unknown name / not an invocation → None (sent to the model as-is).
        assert!(expand_skill(":nope", &skills).is_none());
        assert!(expand_skill("hello :ship", &skills).is_none());
        assert!(expand_skill(": ship", &skills).is_none());
    }

    /// A skill that declares `args:` consumes only its first token as the
    /// argument; any text after it is appended to the body as extra context.
    #[test]
    fn declared_args_skill_splits_arg_from_trailing_context() {
        let mut audit = skill("audit", "", "Audit at depth $ARGUMENTS.");
        audit.args = vec!["low".into(), "high".into()];
        let skills = vec![audit];

        // First token fills $ARGUMENTS; the rest is appended on its own line.
        assert_eq!(
            expand_skill(":audit high focus on the parser", &skills).unwrap(),
            "Audit at depth high.\n\nfocus on the parser"
        );
        // Just the arg, no trailing context: nothing is appended.
        assert_eq!(
            expand_skill(":audit low", &skills).unwrap(),
            "Audit at depth low."
        );
        // No arg at all: $ARGUMENTS renders empty, as before.
        assert_eq!(expand_skill(":audit", &skills).unwrap(), "Audit at depth .");
    }

    /// A skill with `args:` but no `$ARGUMENTS` placeholder still appends the
    /// first token (existing no-placeholder behavior) followed by any extra.
    #[test]
    fn declared_args_skill_without_placeholder_appends_both() {
        let mut s = skill("audit", "", "Run the audit.");
        s.args = vec!["low".into(), "high".into()];
        let skills = vec![s];
        assert_eq!(
            expand_skill(":audit high and check the auth flow", &skills).unwrap(),
            "Run the audit.\n\nhigh\n\nand check the auth flow"
        );
    }

    /// A skill WITHOUT `args:` is unchanged: the whole remainder is one argument
    /// and is not split on the first space (a pasted error, a commit scope).
    #[test]
    fn free_form_skill_keeps_whole_remainder_as_one_argument() {
        let skills = vec![skill("fix", "", "Fix this: $ARGUMENTS")];
        assert_eq!(
            expand_skill(":fix TypeError at line 5 in foo.rs", &skills).unwrap(),
            "Fix this: TypeError at line 5 in foo.rs"
        );
    }

    #[test]
    fn discovery_dedupes_by_name_project_first() {
        let dir = tempfile::tempdir().unwrap();
        let hrdr = dir.path().join(".hrdr/skills");
        let claude = dir.path().join(".claude/commands");
        std::fs::create_dir_all(&hrdr).unwrap();
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(hrdr.join("ship.md"), "hrdr wins").unwrap();
        std::fs::write(claude.join("ship.md"), "claude loses").unwrap();
        std::fs::write(claude.join("review.md"), "review the diff").unwrap();
        std::fs::write(claude.join("notes.txt"), "not a skill").unwrap();

        let skills = discover_skills(dir.path());
        let ship = skills.iter().find(|s| s.name == "ship").unwrap();
        assert_eq!(ship.body, "hrdr wins", "project .hrdr dir outranks .claude");
        assert!(skills.iter().any(|s| s.name == "review"));
        assert!(!skills.iter().any(|s| s.name == "notes"));
    }

    /// The eight built-in templates each parse into a usable skill: a name,
    /// a non-empty description and body, and — for `release`/`review`/`audit`, whose
    /// templates declare `args:` — the completion candidates the popup should
    /// offer after `:name `. `commit`, `fix`, `test`, `todo`, and `plan` declare none, so their lists are empty.
    #[test]
    fn builtins_parse_with_names_descriptions_bodies_and_args() {
        let skills = builtin_skills();
        assert_eq!(
            skills.len(),
            8,
            "audit, commit, fix, plan, release, review, test, todo"
        );

        for name in [
            "audit", "commit", "fix", "plan", "release", "review", "test", "todo",
        ] {
            let s = skills
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("missing built-in {name}"));
            assert!(!s.description.is_empty(), "{name} description");
            assert!(!s.body.is_empty(), "{name} body");
            assert_eq!(s.source, "built-in");
        }

        assert!(
            skills
                .iter()
                .find(|s| s.name == "commit")
                .unwrap()
                .args
                .is_empty(),
            "commit declares no args"
        );
        assert_eq!(
            skills.iter().find(|s| s.name == "release").unwrap().args,
            vec!["patch", "minor", "major"]
        );
        assert_eq!(
            skills.iter().find(|s| s.name == "review").unwrap().args,
            vec!["low", "high"]
        );
        assert_eq!(
            skills.iter().find(|s| s.name == "audit").unwrap().args,
            vec!["low", "high"]
        );
        assert!(
            skills
                .iter()
                .find(|s| s.name == "fix")
                .unwrap()
                .args
                .is_empty(),
            "fix declares no args"
        );
        assert!(
            skills
                .iter()
                .find(|s| s.name == "test")
                .unwrap()
                .args
                .is_empty(),
            "test declares no args"
        );
        assert!(
            skills
                .iter()
                .find(|s| s.name == "todo")
                .unwrap()
                .args
                .is_empty(),
            "todo declares no args"
        );
        assert!(
            skills
                .iter()
                .find(|s| s.name == "plan")
                .unwrap()
                .args
                .is_empty(),
            "plan declares no args"
        );
    }

    /// `discover_skills` on a cwd with no skill directories at all still
    /// returns the eight built-ins — the whole point of shipping them is that
    /// `:commit`/`:release`/`:review`/`:audit`/`:fix`/`:todo`/`:test`/`:plan` work with zero setup.
    #[test]
    fn discover_skills_on_empty_cwd_returns_only_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let skills = discover_skills(dir.path());
        let mut names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "audit", "commit", "fix", "plan", "release", "review", "test", "todo"
            ]
        );
        assert!(skills.iter().all(|s| s.source == "built-in"));
    }

    /// A project's own `.hrdr/skills/commit.md` shadows the built-in `commit`
    /// — built-ins are appended last in `discover_skills`, so they only fill
    /// gaps the dedup (first source wins, case-insensitive) leaves open.
    #[test]
    fn project_skill_overrides_the_builtin_of_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let hrdr = dir.path().join(".hrdr/skills");
        std::fs::create_dir_all(&hrdr).unwrap();
        std::fs::write(hrdr.join("commit.md"), "project commit wins").unwrap();

        let skills = discover_skills(dir.path());
        let commit = skills.iter().find(|s| s.name == "commit").unwrap();
        assert_eq!(commit.body, "project commit wins");
        assert_ne!(commit.source, "built-in");
        // The other seven built-ins are still present, unshadowed.
        assert!(
            skills
                .iter()
                .any(|s| s.name == "release" && s.source == "built-in")
        );
        assert!(
            skills
                .iter()
                .any(|s| s.name == "review" && s.source == "built-in")
        );
    }

    /// A skill dir holding far more than `MAX_SKILLS` files yields a bounded
    /// set: discovery stops at the aggregate file-count cap rather than reading
    /// every file. The project `.hrdr/skills` dir is scanned first and fills the
    /// budget, so the cap bites there (no reliance on the machine's user dirs);
    /// the built-ins are still appended afterwards.
    #[test]
    fn discover_skills_caps_the_file_count() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join(".hrdr/skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        for i in 0..(MAX_SKILLS + 50) {
            std::fs::write(
                skills_dir.join(format!("skill{i:04}.md")),
                format!("Body for skill {i}."),
            )
            .unwrap();
        }
        let skills = discover_skills(dir.path());
        let discovered = skills.iter().filter(|s| s.source != "built-in").count();
        assert_eq!(
            discovered, MAX_SKILLS,
            "skill ingestion must stop at the aggregate file-count cap"
        );
        // The built-ins survive the cap — they're appended unconditionally.
        assert!(
            skills
                .iter()
                .any(|s| s.name == "commit" && s.source == "built-in")
        );
    }

    #[test]
    fn completions_rank_prefix_then_substring_then_description() {
        let skills = vec![
            skill("ship", "release checklist", "…"),
            skill("review", "inspect a shipped diff", "…"),
        ];
        let names = |i: &str| {
            skill_completions(i, &skills)
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(":"), vec![":review", ":ship"]);
        assert_eq!(names(":sh").first().map(String::as_str), Some(":ship"));
        // Description match surfaces :review for "diff".
        assert_eq!(names(":diff"), vec![":review"]);
        // A space kills completion; non-: input yields nothing.
        assert!(names(":ship ").is_empty());
        assert!(names("/ship").is_empty());
    }
}

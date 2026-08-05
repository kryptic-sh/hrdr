//! Skills: reusable prompt templates, invocable by the **user** with a `:`
//! prefix (`:name args…`) and by the **model** through the `skill` tool.
//!
//! A skill is a Markdown file — optional YAML frontmatter (`name:`,
//! `description:`, `args:`, `model_invocable:`), body = the prompt. On invocation the body is sent
//! to the model with `$ARGUMENTS` filled from the text after the skill name: a
//! skill that declares `args:` takes just the first token as its argument and
//! appends any trailing text as extra context, while a skill without `args:`
//! takes the whole remainder (see [`expand_skill`]). Discovery mirrors the
//! sub-agent files: project dirs first, then user dirs, hrdr → Claude Code →
//! opencode conventions, then hrdr's own built-in skills (`:commit`,
//! `:release`, `:review`, `:audit`, `:fix`, `:todo`, `:test`, `:plan`,
//! `:tidy`, `:perf`, `:sweep`) last — deduped by name (first source wins), so a
//! user or project file always overrides a built-in of the same name.
//!
//! This lives in `hrdr-agent` rather than in a frontend because the model can
//! invoke a skill: the agent lists what is available in its system prompt
//! (`prompt::skills_section`) and loads a body on demand through the `skill`
//! tool. Frontend-only concerns — the `:`-completion popup and the `/skills`
//! picker filter — stay in `hrdr-app` on top of this.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// The skills hrdr ships with, baked into the binary via `include_str!` — the
// same convention the prompt fragments use — so a fresh install has a working
// `:commit`, `:release`, `:review`, `:audit`, `:fix`, `:todo`, `:test`,
// `:plan`, `:tidy`, `:perf` with no setup. Content lives in
// `templates/skills/*.md`, not here: keep the prompt text in Markdown
// (reviewable, diffable, editable without touching Rust) and this file to
// parsing/wiring only.
const BUILTIN_COMMIT: &str = include_str!("templates/skills/commit.md");
const BUILTIN_RELEASE: &str = include_str!("templates/skills/release.md");
const BUILTIN_REVIEW: &str = include_str!("templates/skills/review.md");
const BUILTIN_AUDIT: &str = include_str!("templates/skills/audit.md");
const BUILTIN_TODO: &str = include_str!("templates/skills/todo.md");
const BUILTIN_TEST: &str = include_str!("templates/skills/test.md");
const BUILTIN_FIX: &str = include_str!("templates/skills/fix.md");
const BUILTIN_PLAN: &str = include_str!("templates/skills/plan.md");
const BUILTIN_TIDY: &str = include_str!("templates/skills/tidy.md");
const BUILTIN_PERF: &str = include_str!("templates/skills/perf.md");
const BUILTIN_SWEEP: &str = include_str!("templates/skills/sweep.md");

/// Max bytes for a single skill file; files larger than this are skipped.
const MAX_SKILL_FILE_BYTES: u64 = 64 * 1024;

/// Aggregate ceilings on skill ingestion across ALL skill dirs combined: at
/// most this many skill files read, and at most this many total bytes. A real
/// setup has a handful of small skill Markdown files, so these ceilings are
/// far beyond anything genuine — the cap only stops a hostile or accidental
/// directory full of files from making hrdr read unbounded bytes on every `:`
/// input and skill listing. Once either is hit we stop reading and warn; the
/// built-ins are always appended regardless.
const MAX_SKILLS: usize = 256;
const MAX_SKILLS_TOTAL_BYTES: usize = 4 * 1024 * 1024;

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Invocation name (`:name`) — frontmatter `name:`, else the file stem.
    pub name: String,
    /// One-line summary for the completion popup / `/skills` listing, and the
    /// only thing about a skill that the system prompt spends tokens on.
    pub description: String,
    /// The prompt template (the file body).
    pub body: String,
    /// Where it came from, for the `/skills` listing (home-shortened dir).
    pub source: String,
    /// Candidate argument values (frontmatter `args:`, comma-separated or
    /// `[a, b]`), offered by the completion popup after `:name `.
    pub args: Vec<String>,
    /// The model may load this itself (frontmatter `model_invocable:`, default
    /// `true`). `false` keeps it out of the prompt listing and makes the `skill`
    /// tool refuse it: the user's `:name` stays the only way in — for a
    /// procedure whose last step is outward-facing and hard to reverse, deciding
    /// to run it is the user's call, not the model's.
    pub model_invocable: bool,
}

/// The skill directories to scan, in precedence order (highest first).
///
/// `project` decides whether the working tree's three directories are scanned at
/// all — see [`ProjectInstructions`](crate::prompt::ProjectInstructions). They are
/// the worst of the instruction surfaces to load from an untrusted repo: they are
/// discovered **before** the built-ins and shadow them by name, with
/// `model_invocable` defaulting true, so a repo shipping `.hrdr/skills/commit.md`
/// replaces the vetted `:commit` outright.
fn skill_dirs(cwd: &Path, project: crate::prompt::ProjectInstructions) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Project scopes (nearest / most specific) first.
    if project == crate::prompt::ProjectInstructions::Load {
        dirs.push(cwd.join(".hrdr").join("skills"));
        dirs.push(cwd.join(".claude").join("commands"));
        dirs.push(cwd.join(".opencode").join("command"));
    }
    // User scopes.
    if let Some(d) = crate::config_dir() {
        dirs.push(d.join("skills")); // ~/.config/hrdr/skills
    }
    if let Some(home) = crate::agents_dir::home_dir() {
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
pub fn discover_skills(cwd: &Path, project: crate::prompt::ProjectInstructions) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    // Aggregate budget across ALL skill dirs combined. Dirs are scanned in
    // precedence order (project before user), so exhausting the budget drops
    // the least-specific files first.
    let mut file_count: usize = 0;
    let mut total_bytes: usize = 0;
    let mut truncated = false;
    for dir in skill_dirs(cwd, project) {
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
            // corrupt the display. The cap is a defensive ceiling (`MAX_SKILLS` /
            // `MAX_SKILLS_TOTAL_BYTES`) no real setup reaches, so there is nothing
            // actionable to say.
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

/// hrdr's built-in skills — `:commit`, `:release`, `:review`, `:audit`,
/// `:fix`, `:todo`, `:test`, `:plan`, `:tidy`, `:perf` — parsed from
/// the Markdown templates baked into the binary at compile time. Always one
/// entry per template (each is a checked-in, non-empty file, so parsing cannot
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
        (BUILTIN_TIDY, "tidy"),
        (BUILTIN_PERF, "perf"),
        (BUILTIN_SWEEP, "sweep"),
    ]
    .into_iter()
    .filter_map(|(text, stem)| parse_skill_file(text, stem, "built-in"))
    .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Parse one skill file: optional YAML frontmatter (a leading `---` … `---`
/// fence containing `name:` / `description:` / `args:` / `model_invocable:`),
/// body = the prompt. `None` when the body is empty.
pub fn parse_skill_file(text: &str, filename_stem: &str, source: &str) -> Option<Skill> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (fm, body) = match crate::split_fence(text) {
        Some((fm, body)) => (parse_frontmatter(fm), body),
        None => (Frontmatter::default(), text),
    };
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    Some(Skill {
        name: fm.name.unwrap_or_else(|| filename_stem.to_string()),
        description: fm.description.unwrap_or_default(),
        body: body.to_string(),
        source: source.to_string(),
        args: fm.args,
        model_invocable: fm.model_invocable,
    })
}

/// A skill file's frontmatter fields, all optional.
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    args: Vec<String>,
    /// `model_invocable: false` keeps a skill out of the prompt listing and
    /// makes the `skill` tool refuse it — user-invocable only.
    model_invocable: bool,
}

impl Default for Frontmatter {
    fn default() -> Self {
        // Opt-out, not opt-in: an unmarked skill is a procedure the user wants
        // followed, and the whole point of the listing is that the model finds it
        // without being told. `:release` is the exception, and says so in its own
        // frontmatter.
        Self {
            name: None,
            description: None,
            args: Vec::new(),
            model_invocable: true,
        }
    }
}

/// Extract the frontmatter fields from a fence's text via real YAML parsing
/// (`serde_yaml_ng`), rather than the old line-by-line `key: value` scan —
/// which silently dropped anything YAML-legal but not on a single line:
/// prettier wraps a long `description:` onto a continuation line, and block
/// scalars (`description: >` / `|`) or list-form `args:`
/// (`args:\n  - low\n  - high`) never matched at all.
///
/// Malformed YAML (not parseable, or not a mapping — e.g. the frontmatter is
/// a bare scalar or list) degrades gracefully to "no frontmatter" instead of
/// failing the whole skill: `split_fence` has already stripped the fence off
/// the body, so the raw frontmatter text never leaks into the prompt either
/// way, and the caller falls back to a stem-derived name with empty
/// description/args.
fn parse_frontmatter(fm: &str) -> Frontmatter {
    let Ok(serde_yaml_ng::Value::Mapping(map)) = serde_yaml_ng::from_str(fm) else {
        return Frontmatter::default();
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
    // Only a literal `false` opts out. Anything else — absent, a typo, a string —
    // leaves the skill model-invocable: failing open here costs a menu entry the
    // author may not have wanted, while failing closed would silently hide a
    // skill and look like the feature is broken.
    let model_invocable = map.get("model_invocable") != Some(&serde_yaml_ng::Value::Bool(false));
    Frontmatter {
        name,
        description,
        args,
        model_invocable,
    }
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
    Some(expand_body(skill, after_name))
}

/// Fill `skill`'s body from `arguments` — the shared half of [`expand_skill`]
/// and the `skill` tool, so a model-invoked skill and a `:`-invoked one expand
/// byte-identically. `arguments` is everything after the skill name.
///
/// A declared-`args:` skill consumes only its first token as the argument; the
/// rest is appended. A skill without `args:` takes the whole remainder.
pub fn expand_body(skill: &Skill, arguments: &str) -> String {
    let arguments = arguments.trim();
    let (arg, extra) = if skill.args.is_empty() {
        (arguments, "")
    } else {
        let mut split = arguments.splitn(2, char::is_whitespace);
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
    prompt
}

/// The live skill set, shared between the agent — which re-discovers it
/// whenever the cwd changes, so the prompt listing and the tool never disagree
/// — and [`SkillTool`].
pub(crate) type SharedSkills = Arc<Mutex<Vec<Skill>>>;

/// Cap on the bytes one `skill` call returns. Discovery already refuses a skill
/// file over [`MAX_SKILL_FILE_BYTES`], so this only bounds a pathological one; it
/// is deliberately far above `ctx.max_output` because a skill body is a
/// *procedure the model just asked for by name*, and half a procedure is worse
/// than the tokens — the model would follow it anyway. Overflow still spills to
/// a file it can `read`, like every other tool's.
const SKILL_OUTPUT_MAX_BYTES: usize = 24 * 1024;

/// `skill` — load one skill's instructions by name. The names and one-line
/// descriptions are already in the system prompt (`prompt::skills_section`), so
/// this tool is the "now give me the body" half: it costs nothing until the
/// model decides a skill applies.
///
/// Read-only: it reads files discovery already read and returns text. What the
/// *body* then asks for is bounded by the agent's own tool set, exactly as it is
/// when a user types `:name`.
pub(crate) struct SkillTool {
    pub(crate) skills: SharedSkills,
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }

    fn description(&self) -> &'static str {
        "Load a skill — a reusable procedure the user, this project, or hrdr itself wrote for a \
         recurring task (committing, releasing, reviewing, auditing…). The available skills are \
         listed by name and one-line description in your system prompt under `Skills`; this \
         returns the named one's full instructions. \
         Call it BEFORE doing the work whenever the task matches a skill's description — the \
         skill is how the user wants that job done, so following it beats improvising. \
         `arguments` is the free-form text the skill's `$ARGUMENTS` placeholder takes (a depth, a \
         scope, a pasted error); omit it when the skill needs none. \
         Read-only: it returns instructions and changes nothing. Follow them with the tools you \
         already have."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name, as listed under `Skills` in your system prompt (a leading `:` is accepted)."
                },
                "arguments": {
                    "type": "string",
                    "description": "Optional: text filling the skill's $ARGUMENTS placeholder (e.g. `high` for an audit depth)."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(|n| n.trim().trim_start_matches(':'))
            .filter(|n| !n.is_empty())
            .ok_or_else(|| anyhow::anyhow!("skill: `name` is required (a skill name)"))?;
        let arguments = args
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let skills = self
            .skills
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(skill) = skills.iter().find(|s| s.name.eq_ignore_ascii_case(name)) else {
            anyhow::bail!(
                "unknown skill '{name}' — available: {}",
                known_names(&skills)
            );
        };
        // `model_invocable: false` — the user's `:name` is the only way in (it is
        // also absent from the prompt listing, so this is reachable only by a model
        // that guessed the name). Point at the user rather than refusing flatly:
        // asking them to run `:{name}` is the useful next move, and is exactly why
        // the skill is marked.
        if !skill.model_invocable {
            anyhow::bail!(
                "skill '{name}' is user-invocable only (`model_invocable: false`), so you cannot \
                 load it. If the task needs it, ask the user to run `:{name}` themselves."
            );
        }
        // Name the source: a built-in is hrdr's own text, anything else came off
        // disk from this project or the user's config. Same trust as `AGENTS.md`,
        // and stated so the model knows whose procedure it is following.
        let body = format!(
            "Skill `{}` (source: {}) — instructions from the user or this project; follow them \
             for this task.\n\n{}",
            skill.name,
            skill.source,
            expand_body(skill, arguments)
        );
        Ok(hrdr_tools::truncate_saved(
            &body,
            SKILL_OUTPUT_MAX_BYTES,
            usize::MAX,
            hrdr_tools::TruncateSide::Head,
            "skill",
        ))
    }
}

/// Skill names for an unknown-name error: all of them while that is a readable
/// list, otherwise the first few plus a count — a wall of names is how a
/// half-remembered name gets matched onto the wrong skill.
fn known_names(skills: &[Skill]) -> String {
    const SHOWN: usize = 40;
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    if names.len() <= SHOWN {
        return names.join(", ");
    }
    format!(
        "{}, … ({} more)",
        names[..SHOWN].join(", "),
        names.len() - SHOWN
    )
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
            model_invocable: true,
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
    /// prompt body sent to the model — covered by [`crate::split_fence`]'s
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

    /// The `skill` tool and a `:` invocation share [`expand_body`], so the same
    /// skill and the same argument text expand to the same bytes whichever way
    /// it was invoked.
    #[test]
    fn expand_body_matches_a_colon_invocation() {
        let mut audit = skill("audit", "", "Audit at depth $ARGUMENTS.");
        audit.args = vec!["low".into(), "high".into()];
        let skills = vec![audit.clone()];
        assert_eq!(
            expand_body(&audit, "high focus on the parser"),
            expand_skill(":audit high focus on the parser", &skills).unwrap()
        );
        assert_eq!(
            expand_body(&audit, ""),
            expand_skill(":audit", &skills).unwrap()
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

        let skills = discover_skills(dir.path(), crate::prompt::ProjectInstructions::Load);
        let ship = skills.iter().find(|s| s.name == "ship").unwrap();
        assert_eq!(ship.body, "hrdr wins", "project .hrdr dir outranks .claude");
        assert!(skills.iter().any(|s| s.name == "review"));
        assert!(!skills.iter().any(|s| s.name == "notes"));
    }

    /// The built-in templates each parse into a usable skill: a name,
    /// a non-empty description and body, and — for `release`/`review`/`audit`, whose
    /// templates declare `args:` — the completion candidates the popup should
    /// offer after `:name `. The rest declare none, so their lists are empty.
    #[test]
    fn builtins_parse_with_names_descriptions_bodies_and_args() {
        let skills = builtin_skills();
        assert_eq!(
            skills.len(),
            11,
            "audit, commit, fix, perf, plan, release, review, sweep, test, tidy, todo"
        );

        for name in [
            "audit", "commit", "fix", "perf", "plan", "release", "review", "sweep", "test", "tidy",
            "todo",
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
        for name in ["fix", "test", "todo", "plan", "tidy", "perf", "sweep"] {
            assert!(
                skills
                    .iter()
                    .find(|s| s.name == name)
                    .unwrap()
                    .args
                    .is_empty(),
                "{name} declares no args"
            );
        }
    }

    /// `discover_skills` on a cwd with no skill directories at all still
    /// returns the built-ins — the whole point of shipping them is that
    /// `:commit`/`:release`/`:review`/`:audit`/`:fix`/`:todo`/`:test`/`:plan`/`:tidy`/`:perf`/`:sweep`
    /// work with zero setup.
    #[test]
    fn discover_skills_on_empty_cwd_returns_only_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let skills = discover_skills(dir.path(), crate::prompt::ProjectInstructions::Load);
        let mut names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "audit", "commit", "fix", "perf", "plan", "release", "review", "sweep", "test",
                "tidy", "todo"
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

        let skills = discover_skills(dir.path(), crate::prompt::ProjectInstructions::Load);
        let commit = skills.iter().find(|s| s.name == "commit").unwrap();
        assert_eq!(commit.body, "project commit wins");
        assert_ne!(commit.source, "built-in");
        // The other built-ins are still present, unshadowed.
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

    /// The tool that makes skills model-invocable: it resolves a name against the
    /// shared set and returns the expanded body, framed with its source so the
    /// model knows whose procedure it is following.
    #[tokio::test]
    async fn skill_tool_loads_a_body_and_fills_arguments() {
        use hrdr_tools::Tool;
        let mut audit = skill("audit", "audit the code", "Audit at depth $ARGUMENTS.");
        audit.args = vec!["low".into(), "high".into()];
        let tool = SkillTool {
            skills: Arc::new(Mutex::new(vec![audit])),
        };
        let ctx = hrdr_tools::ToolContext::new(std::env::temp_dir());

        let out = tool
            .execute(
                serde_json::json!({"name": "audit", "arguments": "high"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("Audit at depth high."), "{out}");
        assert!(out.contains("Skill `audit` (source: test)"), "{out}");

        // A leading `:` is accepted (the model sees `:audit` in the user's habits)
        // and the name matches case-insensitively, like a `:` invocation.
        let out = tool
            .execute(serde_json::json!({"name": ":AUDIT"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("Audit at depth ."), "{out}");

        // Read-only: a read-only sub-agent keeps it.
        assert!(tool.read_only());
    }

    /// `model_invocable: false` is a boundary, not a hint: the tool refuses even
    /// when the model guesses the name (the listing never showed it), and points
    /// at the user's `:name` instead.
    #[tokio::test]
    async fn skill_tool_refuses_a_user_only_skill() {
        use hrdr_tools::Tool;
        let mut release = skill("release", "cut a release", "Bump, tag, push.");
        release.model_invocable = false;
        let tool = SkillTool {
            skills: Arc::new(Mutex::new(vec![release])),
        };
        let ctx = hrdr_tools::ToolContext::new(std::env::temp_dir());
        let err = tool
            .execute(serde_json::json!({"name": "release"}), &ctx)
            .await
            .unwrap_err();
        let shown = format!("{err:#}");
        assert!(shown.contains("user-invocable only"), "{shown}");
        assert!(shown.contains("`:release`"), "points at the user: {shown}");
        assert!(!shown.contains("Bump, tag, push"), "no body leaks: {shown}");

        // Every shipped built-in is model-invocable, `:release` included — the
        // user's 2026-08-05 reversal of the marking it used to carry.
        let builtins = builtin_skills();
        let flag = |name: &str| {
            builtins
                .iter()
                .find(|s| s.name == name)
                .unwrap()
                .model_invocable
        };
        for name in [
            "audit", "commit", "fix", "perf", "plan", "release", "review", "test", "tidy", "todo",
        ] {
            assert!(flag(name), "{name} stays model-invocable");
        }
    }

    /// The frontmatter flag fails **open**: only a literal `false` opts out, so a
    /// typo or a stray string leaves the skill loadable rather than silently
    /// hiding it.
    #[test]
    fn model_invocable_only_a_literal_false_opts_out() {
        let parse = |fm: &str| {
            parse_skill_file(&format!("---\n{fm}\n---\nBody."), "s", "src")
                .unwrap()
                .model_invocable
        };
        assert!(!parse("model_invocable: false"));
        assert!(parse("model_invocable: true"));
        assert!(parse("name: s"), "absent → invocable");
        assert!(
            parse("model_invocable: \"false\""),
            "a string is not `false`"
        );
        assert!(
            parse("model_invocible: false"),
            "a typo'd key is not the key"
        );
        // No frontmatter at all is the common case: invocable.
        assert!(
            parse_skill_file("Just a body.", "s", "src")
                .unwrap()
                .model_invocable
        );
    }

    /// An unknown name is an error that names what *is* available — the model
    /// half-remembering `:commits` must land on the list, not on nothing.
    #[tokio::test]
    async fn skill_tool_rejects_an_unknown_name_and_lists_the_known_ones() {
        use hrdr_tools::Tool;
        let tool = SkillTool {
            skills: Arc::new(Mutex::new(vec![skill("commit", "", "body")])),
        };
        let ctx = hrdr_tools::ToolContext::new(std::env::temp_dir());
        let err = tool
            .execute(serde_json::json!({"name": "commits"}), &ctx)
            .await
            .unwrap_err();
        let shown = format!("{err:#}");
        assert!(shown.contains("unknown skill 'commits'"), "{shown}");
        assert!(shown.contains("available: commit"), "{shown}");

        // A missing/blank name is an error too, not a silent first-skill pick.
        assert!(
            tool.execute(serde_json::json!({"name": "  "}), &ctx)
                .await
                .is_err()
        );
        assert!(tool.execute(serde_json::json!({}), &ctx).await.is_err());
    }

    /// The unknown-name list is bounded: a directory of many skills must not dump
    /// every name into one error, which is how a half-remembered name gets matched
    /// onto the wrong skill.
    #[test]
    fn unknown_name_list_is_bounded() {
        let many: Vec<Skill> = (0..100)
            .map(|i| skill(&format!("s{i:03}"), "", "b"))
            .collect();
        let listed = known_names(&many);
        assert!(listed.contains("s000"));
        assert!(listed.contains("(60 more)"), "{listed}");
        assert!(!listed.contains("s099"), "the tail is counted, not named");
        // A readable list is shown whole.
        assert_eq!(known_names(&many[..3]), "s000, s001, s002");
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
        let skills = discover_skills(dir.path(), crate::prompt::ProjectInstructions::Load);
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
}

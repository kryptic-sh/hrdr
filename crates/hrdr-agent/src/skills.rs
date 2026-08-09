//! Skills: Agent Skill bundles — a **directory** holding a `SKILL.md` plus
//! whatever that file references (`references/`, `scripts/`, `assets/`, …),
//! invocable by the **user** with a `:` prefix (`:name`) and by the **model**
//! through the `skill` tool.
//!
//! This is the cross-vendor format Claude Code, Codex and opencode all read, so
//! hrdr reads the same bundles rather than a dialect of its own: frontmatter
//! `name:` and `description:` are required, `name` must be
//! `^[a-z0-9]+(-[a-z0-9]+)*$` and equal the directory holding `SKILL.md`, and
//! `description` is 1–[`MAX_DESCRIPTION_LEN`] characters. A bundle that breaks
//! any of those is **skipped**, with the reason kept for the `/commands` picker
//! to show — repairing one would mean guessing what its author meant.
//!
//! Skills take **no arguments**: there is no `$ARGUMENTS` substitution and no
//! `args:` frontmatter, matching the standard. Text typed after `:name` is
//! appended to the body as free-form context (see [`skill_prompt`]).
//!
//! Deliberately parallel to [`crate::commands`] rather than merged with it: a
//! command is one flat `<name>.md` file with `$ARGUMENTS` and a `model_invocable`
//! switch, a skill is a validated directory bundle whose sibling files are half
//! the point. They share one `:name` namespace, and **a command wins** a
//! collision ([`expand_invocation`]); the shadowed skill stays loadable through
//! the `skill` tool.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The one file name that makes a directory a skill. Capitalized, as every
/// implementation of the format spells it.
const SKILL_FILENAME: &str = "SKILL.md";

/// Max bytes for a single `SKILL.md`; larger files are skipped.
const MAX_SKILL_FILE_BYTES: u64 = 64 * 1024;

/// Aggregate ceilings on skill ingestion across ALL skill roots combined, for
/// the same reason [`crate::commands`] has them: discovery runs inside the TUI
/// on every cwd change, so a hostile or accidental directory tree must not make
/// hrdr read unbounded bytes. Far beyond any real setup.
const MAX_SKILLS: usize = 256;
const MAX_SKILLS_TOTAL_BYTES: usize = 4 * 1024 * 1024;

/// How deep a skill root is walked. Bundles are normally one level down
/// (`skills/<name>/SKILL.md`); the extra depth is for the grouping directories
/// codex and opencode allow (`skills/<group>/<name>/SKILL.md`).
const SKILL_MAX_DEPTH: usize = 6;

/// codex's own override for its user directory, honoured so one bundle can serve
/// both harnesses.
const CODEX_HOME: &str = "CODEX_HOME";

/// Longest `name`, in characters — codex's `MAX_NAME_LEN`, and what the format's
/// documented "1–64 characters" rule means.
const MAX_NAME_LEN: usize = 64;

/// Longest `description`, in characters — codex's `MAX_DESCRIPTION_LEN`, and
/// opencode's documented "1-1024 characters".
const MAX_DESCRIPTION_LEN: usize = 1024;

/// One discovered skill bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Invocation name (`:name`), from frontmatter `name:` — validated to equal
    /// the directory that holds the `SKILL.md`, so the two can never disagree.
    pub name: String,
    /// One-line summary. Required by the format, because it is the only thing
    /// the model has to decide whether the skill applies.
    pub description: String,
    /// The `SKILL.md` body (frontmatter stripped), trimmed.
    pub body: String,
    /// Which root it came from, for the `/commands` listing (home-shortened).
    pub source: String,
    /// The directory holding the `SKILL.md` — absolute, and the anchor every
    /// relative path inside the body (`scripts/x.py`, `references/spec.md`)
    /// resolves against. Named in the tool's output for exactly that reason.
    pub base_dir: PathBuf,
    /// Frontmatter `license:` — parsed and preserved because the format defines
    /// it and a round-trip must not drop it. hrdr does not render it anywhere
    /// yet.
    pub license: Option<String>,
    /// Frontmatter `compatibility:` — same standing as `license`.
    pub compatibility: Option<String>,
    /// Frontmatter `metadata:`, a string→string map (opencode's contract).
    /// Preserved, not rendered.
    pub metadata: BTreeMap<String, String>,
}

/// A `SKILL.md` that failed validation, kept so the `/commands` picker can show
/// it with its reason instead of leaving the author guessing why their skill is
/// missing. Never loadable — an invalid bundle is skipped, not repaired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSkill {
    /// The directory name the bundle sits in — the only name it reliably has,
    /// since `name:` may be the very field that is wrong.
    pub name: String,
    /// The offending `SKILL.md`, home-shortened for display.
    pub path: String,
    /// Why it was skipped, in one line.
    pub reason: String,
}

/// What [`discover_skills`] found: the usable bundles, the ones a
/// higher-precedence root already claimed the name of, and the ones skipped with
/// their reasons. All three are returned because a skill that silently does not
/// appear is the format's most common complaint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveredSkills {
    pub skills: Vec<Skill>,
    /// Bundles that lost a name collision — a lower-precedence root's copy of a
    /// name `skills` already holds. Never invocable and never listed to the
    /// model; kept only so the `/commands` picker can say why they never run.
    pub shadowed: Vec<Skill>,
    pub invalid: Vec<InvalidSkill>,
}

/// The skill roots to scan, in precedence order (highest first): the project's
/// own, then the user's. Each holds `<name>/SKILL.md`, possibly under a grouping
/// directory.
///
/// hrdr's own directory comes first in each scope; the rest are the other
/// harnesses' conventions, read as-is so one bundle serves all of them.
/// `$CODEX_HOME` is honoured (codex's own override), defaulting to `~/.codex`.
///
/// `project` decides whether the working tree's own directories are scanned at
/// all — see [`ProjectInstructions`](crate::prompt::ProjectInstructions), and
/// [`crate::commands::discover_commands`] for why an untrusted repo contributes
/// no prompt text. User roots are always scanned.
///
/// Also the exact set [`crate::Agent`] grants read access to in **every** sandbox
/// mode: a jailed agent that can see a skill listing it cannot read is worse
/// than useless. One definition, so the grant and the discovery cannot drift.
pub fn skill_dirs(cwd: &Path, project: crate::prompt::ProjectInstructions) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if project == crate::prompt::ProjectInstructions::Load {
        dirs.push(cwd.join(".hrdr").join("skills"));
        dirs.push(cwd.join(".agents").join("skills"));
        dirs.push(cwd.join(".claude").join("skills"));
        dirs.push(cwd.join(".codex").join("skills"));
    }
    if let Some(d) = crate::config_dir() {
        dirs.push(d.join("skills")); // ~/.config/hrdr/skills
    }
    if let Some(home) = crate::agents_dir::home_dir() {
        dirs.push(home.join(".agents").join("skills"));
        dirs.push(home.join(".claude").join("skills"));
        // `$CODEX_HOME/skills`, codex's own user location.
        let codex_home = std::env::var_os(CODEX_HOME)
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| home.join(".codex"));
        dirs.push(codex_home.join("skills"));
    }
    dirs
}

/// The key two skill names are compared by — names are lowercase by validation,
/// so this only makes a user's `:NAME` reach the same bundle.
fn skill_match_key(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// Discover skill bundles across the hrdr/agents/Claude/codex roots, relative to
/// `cwd` for project scopes. Each root is walked recursively for `SKILL.md`
/// files; the skill's identity comes from the directory immediately containing
/// it. One skill per name, first root in precedence order wins; the losers go to
/// [`DiscoveredSkills::shadowed`] rather than being dropped.
pub fn discover_skills(
    cwd: &Path,
    project: crate::prompt::ProjectInstructions,
) -> DiscoveredSkills {
    let mut out = DiscoveredSkills::default();
    // Aggregate budget across ALL roots combined, spent per file visited, so a
    // deep tree cannot buy itself more of it.
    let mut file_count: usize = 0;
    let mut total_bytes: usize = 0;
    let mut truncated = false;
    for dir in skill_dirs(cwd, project) {
        if !dir.is_dir() {
            continue;
        }
        let source = crate::display_dir(&dir);
        let mut found: Vec<Skill> = Vec::new();
        // A skill root is usually itself hidden (`.hrdr/`, `.claude/`) and often
        // gitignored, so none of `ignore`'s standard filters belong here — they
        // would drop the very files being looked for. Symlinks are not followed
        // (the builder's default), which is what keeps a symlink loop from
        // turning this walk into a hang.
        for entry in ignore::WalkBuilder::new(&dir)
            .standard_filters(false)
            .max_depth(Some(SKILL_MAX_DEPTH))
            .sort_by_file_path(Path::cmp)
            .build()
            .flatten()
        {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) != Some(SKILL_FILENAME) {
                continue;
            }
            if path.metadata().map(|m| m.len()).unwrap_or(0) > MAX_SKILL_FILE_BYTES {
                continue;
            }
            if file_count >= MAX_SKILLS || total_bytes >= MAX_SKILLS_TOTAL_BYTES {
                truncated = true;
                break;
            }
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            file_count += 1;
            total_bytes = total_bytes.saturating_add(text.len());
            let Some(base_dir) = path.parent() else {
                continue;
            };
            let Some(dir_name) = base_dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            match parse_skill_file(&text, dir_name, &source, base_dir) {
                Ok(skill) => found.push(skill),
                // Silent on stderr on purpose, like `discover_commands`: this runs
                // inside the TUI, so writing there would corrupt the display. The
                // reason travels in the returned list instead, which is what the
                // `/commands` picker shows.
                Err(reason) => out.invalid.push(InvalidSkill {
                    name: dir_name.to_string(),
                    path: crate::display_dir(path),
                    reason,
                }),
            }
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        for skill in found {
            let key = skill_match_key(&skill.name);
            if out.skills.iter().any(|s| skill_match_key(&s.name) == key) {
                // The name is already taken by a higher-precedence root. Kept
                // rather than dropped: this bundle can never run, and the
                // `/commands` picker is where that stops being a mystery.
                out.shadowed.push(skill);
            } else {
                out.skills.push(skill);
            }
        }
        // Merge this root's finds before stopping, so nothing already read is lost.
        if truncated {
            break;
        }
    }
    out
}

/// Parse and validate one `SKILL.md`. `dir_name` is the directory holding it —
/// the name `name:` must equal — and `base_dir` that directory's path.
///
/// `Err` carries the one-line reason the bundle is skipped, for
/// [`InvalidSkill`]. Every rule here is one codex and opencode both enforce; a
/// field neither recognizes is ignored rather than rejected, so a bundle
/// carrying another harness's extras still loads.
pub fn parse_skill_file(
    text: &str,
    dir_name: &str,
    source: &str,
    base_dir: &Path,
) -> Result<Skill, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (fm, body) = crate::split_fence(text).ok_or_else(|| {
        "missing YAML frontmatter (`name` and `description` are required)".to_string()
    })?;
    let serde_yaml_ng::Value::Mapping(map) =
        serde_yaml_ng::from_str(fm).map_err(|e| format!("invalid YAML frontmatter: {e}"))?
    else {
        return Err("frontmatter is not a YAML mapping".to_string());
    };
    let scalar = |key: &str| -> Option<String> {
        map.get(key)
            .and_then(crate::commands::scalar_to_string)
            .filter(|v| !v.is_empty())
    };

    let name = scalar("name").ok_or_else(|| "missing `name`".to_string())?;
    if name.chars().count() > MAX_NAME_LEN {
        return Err(format!("`name` is longer than {MAX_NAME_LEN} characters"));
    }
    if !is_valid_skill_name(&name) {
        return Err(format!(
            "`name` must be lowercase alphanumerics with single hyphen separators \
             (^[a-z0-9]+(-[a-z0-9]+)*$), got `{name}`"
        ));
    }
    if name != dir_name {
        return Err(format!(
            "`name` is `{name}` but the directory is `{dir_name}` — they must match"
        ));
    }

    let description = scalar("description").ok_or_else(|| "missing `description`".to_string())?;
    if description.chars().count() > MAX_DESCRIPTION_LEN {
        return Err(format!(
            "`description` is longer than {MAX_DESCRIPTION_LEN} characters"
        ));
    }

    let metadata = match map.get("metadata") {
        Some(serde_yaml_ng::Value::Mapping(m)) => m
            .iter()
            .filter_map(|(k, v)| {
                Some((
                    crate::commands::scalar_to_string(k)?,
                    crate::commands::scalar_to_string(v)?,
                ))
            })
            .collect(),
        _ => BTreeMap::new(),
    };

    Ok(Skill {
        name,
        description,
        body: body.trim().to_string(),
        source: source.to_string(),
        base_dir: base_dir.to_path_buf(),
        license: scalar("license"),
        compatibility: scalar("compatibility"),
        metadata,
    })
}

/// `^[a-z0-9]+(-[a-z0-9]+)*$`, hand-checked because this crate has no regex
/// dependency and the pattern reads directly: lowercase alphanumeric segments
/// joined by single hyphens, so no leading or trailing `-` and no `--`.
fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// The prompt a skill sends: its body, any free-form `trailing` text the user
/// typed after `:name`, and the base-directory footer.
///
/// **The footer is not decoration.** A bundle's whole point is the files beside
/// its `SKILL.md`, and the body refers to them relatively (`scripts/build.sh`).
/// Without the base directory stated, a relative path resolves against the
/// agent's cwd, which is some other project entirely.
///
/// Shared by the `:name` path and the `skill` tool, so a user-invoked skill and
/// a model-loaded one are byte-identical below the tool's own header.
pub fn skill_prompt(skill: &Skill, trailing: &str) -> String {
    let mut out = skill.body.clone();
    let trailing = trailing.trim();
    if !trailing.is_empty() {
        // Appended, never substituted: skills declare no parameters, so there is
        // nothing to fill — this is extra context for the procedure.
        out.push_str("\n\n");
        out.push_str(trailing);
    }
    out.push_str(&format!(
        "\n\nBase directory for this skill: {}\nRelative paths in this skill (e.g. `scripts/`, \
         `references/`) are relative to that directory — resolve them against it before reading \
         or running anything.",
        skill.base_dir.display()
    ));
    out
}

/// Expand a `:name …` invocation against the shared namespace: commands first,
/// then skills. `None` when the input is not a `:` invocation or names neither.
///
/// **Commands win a collision.** A command is the more specific thing — the user
/// or this project wrote it for hrdr, arguments and all — and a skill is a
/// portable bundle that may have arrived from anywhere. The shadowed skill is
/// still loadable through the `skill` tool, and the `/commands` picker marks it
/// so the collision is visible rather than mysterious.
pub fn expand_invocation(
    input: &str,
    commands: &[crate::Command],
    skills: &[Skill],
) -> Option<String> {
    if let Some(prompt) = crate::expand_command(input, commands) {
        return Some(prompt);
    }
    let (name, rest) = crate::commands::split_invocation(input)?;
    let key = skill_match_key(name);
    let skill = skills.iter().find(|s| skill_match_key(&s.name) == key)?;
    Some(skill_prompt(skill, rest))
}

/// The live skill set, shared between the agent — which re-discovers it whenever
/// the cwd changes, so the prompt listing and the tool never disagree — and
/// [`SkillTool`].
pub(crate) type SharedSkills = Arc<Mutex<Vec<Skill>>>;

/// Cap on the bytes one `skill` call returns, matching the `command` tool's: a
/// skill body is a procedure the model just asked for by name, and half a
/// procedure is worse than the tokens. Overflow spills to a file it can `read`.
const SKILL_OUTPUT_MAX_BYTES: usize = 24 * 1024;

/// `skill` — load one skill bundle's instructions by name. The names and
/// descriptions are already in the system prompt (`prompt::skills_section`), so
/// this is the "now give me the body" half.
///
/// Read-only: it reads a file discovery already read and returns text. What the
/// body then asks for is bounded by the agent's own tool set — including a
/// bundled `scripts/`, which runs through `shell` under whatever the current
/// sandbox mode already permits, with no privilege of its own.
pub(crate) struct SkillTool {
    pub(crate) skills: SharedSkills,
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }

    fn description(&self) -> &'static str {
        "Load a skill — a reusable procedure bundle (an Agent Skill: a `SKILL.md` plus the \
         scripts, references and assets beside it) written by the user, this project, or a \
         third party. The available skills are listed by name and one-line description in your \
         system prompt under `Skills`; this returns the named one's full instructions plus its \
         base directory. \
         Call it BEFORE doing the work whenever the task matches a skill's description — the \
         skill is how that job is meant to be done here. Skills take no arguments. \
         Read-only: it returns instructions and changes nothing. Follow them with the tools you \
         already have; a bundled script is run through `shell` like any other command."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name, as listed under `Skills` in your system prompt (a leading `:` is accepted)."
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
        let skills = self
            .skills
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let key = skill_match_key(name);
        let Some(skill) = skills.iter().find(|s| skill_match_key(&s.name) == key) else {
            anyhow::bail!(
                "unknown skill '{name}' — available: {}",
                known_names(&skills)
            );
        };
        // Name the source: these bytes came off disk from this project or the
        // user's config, same trust as `AGENTS.md`, and stated so the model knows
        // whose procedure it is following.
        let body = format!(
            "Skill `{}` (source: {}) — a procedure bundle from the user or this project; follow \
             it for this task.\n\n{}",
            skill.name,
            skill.source,
            skill_prompt(skill, "")
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
/// list, otherwise the first few plus a count.
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

    /// Serializes the one test that mutates `$CODEX_HOME`. The variable is
    /// process-global, so two of these at once would read each other's value —
    /// and the failure would be a *passing* test, not a crash.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Write `<root>/<name>/SKILL.md` with `text`, returning the bundle dir.
    fn bundle(root: &Path, name: &str, text: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SKILL_FILENAME), text).unwrap();
        dir
    }

    fn skill_md(name: &str, description: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}")
    }

    /// A valid `SKILL.md` for `name`, padded with body text to exactly `bytes`.
    fn skill_md_of_size(name: &str, bytes: usize) -> String {
        let head = skill_md(name, "d", "");
        let mut text = head.clone();
        text.push_str(&"b".repeat(bytes - head.len()));
        text
    }

    fn parse(dir_name: &str, text: &str) -> Result<Skill, String> {
        parse_skill_file(
            text,
            dir_name,
            "src",
            Path::new("/tmp/skills").join(dir_name).as_path(),
        )
    }

    /// A valid bundle loads with its fields, its base directory, and the
    /// optional frontmatter the format defines (`license`, `compatibility`,
    /// `metadata`) preserved rather than dropped.
    #[test]
    fn a_valid_bundle_parses_with_its_base_dir_and_optional_fields() {
        let s = parse(
            "git-release",
            "---\nname: git-release\ndescription: Create consistent releases\nlicense: MIT\n\
             compatibility: opencode\nmetadata:\n  audience: maintainers\n---\nDraft the notes.",
        )
        .unwrap();
        assert_eq!(s.name, "git-release");
        assert_eq!(s.description, "Create consistent releases");
        assert_eq!(s.body, "Draft the notes.");
        assert_eq!(s.base_dir, Path::new("/tmp/skills/git-release"));
        assert_eq!(s.license.as_deref(), Some("MIT"));
        assert_eq!(s.compatibility.as_deref(), Some("opencode"));
        assert_eq!(
            s.metadata.get("audience").map(String::as_str),
            Some("maintainers")
        );
        // An unknown field is ignored, not rejected — another harness's extras
        // must not make a bundle unloadable here.
        assert!(
            parse(
                "x",
                "---\nname: x\ndescription: d\nallowed-tools: Read\n---\nBody."
            )
            .is_ok()
        );
    }

    /// Each validation rule rejects, and says why. A skipped bundle is never
    /// repaired: `name` disagreeing with its directory is an authoring mistake
    /// with two possible fixes, and guessing which is how `:name` starts
    /// resolving to the wrong procedure.
    #[test]
    fn validation_rejects_and_explains() {
        let cases: [(&str, &str, &str); 7] = [
            ("x", "No frontmatter at all.", "missing YAML frontmatter"),
            ("x", "---\njust a string\n---\nBody.", "not a YAML mapping"),
            ("x", "---\ndescription: d\n---\nBody.", "missing `name`"),
            ("x", "---\nname: x\n---\nBody.", "missing `description`"),
            (
                "My-Skill",
                "---\nname: My-Skill\ndescription: d\n---\nBody.",
                "lowercase alphanumerics",
            ),
            (
                "a--b",
                "---\nname: a--b\ndescription: d\n---\nBody.",
                "lowercase alphanumerics",
            ),
            (
                "other",
                "---\nname: mine\ndescription: d\n---\nBody.",
                "they must match",
            ),
        ];
        for (dir, text, expected) in &cases {
            let err = parse(dir, text).unwrap_err();
            assert!(err.contains(expected), "{dir}: {err}");
        }
        // Over-long `description` and over-long `name`, built from the caps
        // themselves so the test tracks them.
        let long_desc = "d".repeat(MAX_DESCRIPTION_LEN + 1);
        let err = parse("long", &skill_md("long", &long_desc, "Body.")).unwrap_err();
        assert!(err.contains("`description` is longer than"), "{err}");
        let long_name = "n".repeat(MAX_NAME_LEN + 1);
        let err = parse(&long_name, &skill_md(&long_name, "d", "Body.")).unwrap_err();
        assert!(err.contains("`name` is longer than"), "{err}");
        // …and the boundary cases are accepted, so the caps are off-by-one clean.
        assert!(
            parse(
                "long",
                &skill_md("long", &"d".repeat(MAX_DESCRIPTION_LEN), "B.")
            )
            .is_ok()
        );
        let max_name = "n".repeat(MAX_NAME_LEN);
        assert!(parse(&max_name, &skill_md(&max_name, "d", "B.")).is_ok());
    }

    /// The name rule, directly: the shapes the regex accepts and the ones it
    /// does not.
    #[test]
    fn skill_name_rule_matches_the_documented_regex() {
        for ok in ["a", "git-release", "pdf2md", "a1-b2-c3", "0"] {
            assert!(is_valid_skill_name(ok), "{ok} should be valid");
        }
        for bad in ["", "-a", "a-", "a--b", "A", "a_b", "a b", "a.b", "a/b"] {
            assert!(!is_valid_skill_name(bad), "{bad} should be invalid");
        }
    }

    /// Discovery walks each root, names a bundle by its directory, keeps the
    /// invalid ones with their reason, and lets the first root in precedence
    /// order win a name.
    #[test]
    fn discovery_dedupes_by_name_project_first_and_reports_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let hrdr = dir.path().join(".hrdr/skills");
        let claude = dir.path().join(".claude/skills");
        bundle(&hrdr, "ship", &skill_md("ship", "hrdr's ship", "hrdr wins"));
        bundle(
            &claude,
            "ship",
            &skill_md("ship", "claude's ship", "claude loses"),
        );
        bundle(
            &claude,
            "review",
            &skill_md("review", "review a diff", "Review it."),
        );
        bundle(&claude, "broken", &skill_md("mismatch", "d", "Body."));

        let found = discover_skills(dir.path(), crate::prompt::ProjectInstructions::Load);
        let ship = found.skills.iter().find(|s| s.name == "ship").unwrap();
        assert_eq!(
            ship.body, "hrdr wins",
            "project .hrdr root outranks .claude"
        );
        assert_eq!(ship.base_dir, hrdr.join("ship"));
        assert!(found.skills.iter().any(|s| s.name == "review"));
        assert!(!found.skills.iter().any(|s| s.name == "broken"));
        let invalid = found
            .invalid
            .iter()
            .find(|i| i.name == "broken")
            .expect("the mismatched bundle is reported, not silently dropped");
        assert!(invalid.reason.contains("they must match"), "{invalid:?}");
        assert!(invalid.path.ends_with("SKILL.md"), "{invalid:?}");
    }

    /// A grouping directory is allowed — the walk is recursive, and identity
    /// still comes from the directory immediately containing `SKILL.md`.
    #[test]
    fn a_grouping_directory_does_not_change_a_skills_name() {
        let dir = tempfile::tempdir().unwrap();
        let group = dir.path().join(".hrdr/skills/docs");
        bundle(
            &group,
            "pdf-fill",
            &skill_md("pdf-fill", "fill a pdf", "Do it."),
        );

        let found = discover_skills(dir.path(), crate::prompt::ProjectInstructions::Load);
        assert!(
            found.skills.iter().any(|s| s.name == "pdf-fill"),
            "{:?}",
            found.skills
        );
    }

    /// An untrusted repo contributes no skills: with project instructions
    /// skipped, the working tree's roots are not scanned at all.
    #[test]
    fn project_roots_are_skipped_when_project_instructions_are() {
        let dir = tempfile::tempdir().unwrap();
        bundle(
            &dir.path().join(".hrdr/skills"),
            "ship",
            &skill_md("ship", "d", "Body."),
        );
        let found = discover_skills(dir.path(), crate::prompt::ProjectInstructions::Skip);
        assert!(!found.skills.iter().any(|s| s.name == "ship"));
    }

    /// A command wins a name collision: `:name` runs the command, while the
    /// skill stays loadable by the tool (it is still in the list handed here).
    #[test]
    fn a_command_shadows_a_skill_of_the_same_name() {
        let commands = vec![crate::Command {
            name: "ship".to_string(),
            description: "the command".to_string(),
            body: "Run the command.".to_string(),
            source: "test".to_string(),
            args: Vec::new(),
            model_invocable: true,
        }];
        let skills = vec![Skill {
            name: "ship".to_string(),
            description: "the skill".to_string(),
            body: "Run the skill.".to_string(),
            source: "test".to_string(),
            base_dir: PathBuf::from("/tmp/skills/ship"),
            license: None,
            compatibility: None,
            metadata: BTreeMap::new(),
        }];
        let out = expand_invocation(":ship", &commands, &skills).unwrap();
        assert_eq!(out, "Run the command.");
        assert!(!out.contains("Run the skill."));
        // With no command of that name, the skill answers.
        assert!(
            expand_invocation(":ship", &[], &skills)
                .unwrap()
                .contains("Run the skill.")
        );
    }

    /// Text after `:name` is APPENDED, never substituted: skills declare no
    /// parameters, so a `$ARGUMENTS` in the body is literal text the author
    /// wrote, not a placeholder.
    #[test]
    fn trailing_text_is_appended_not_substituted() {
        let skills = vec![Skill {
            name: "audit".to_string(),
            description: "audit it".to_string(),
            body: "Audit the tree. $ARGUMENTS".to_string(),
            source: "test".to_string(),
            base_dir: PathBuf::from("/tmp/skills/audit"),
            license: None,
            compatibility: None,
            metadata: BTreeMap::new(),
        }];
        let out = expand_invocation(":audit focus on the parser", &[], &skills).unwrap();
        assert!(out.contains("Audit the tree. $ARGUMENTS"), "{out}");
        assert!(out.contains("\n\nfocus on the parser\n"), "{out}");
        // …and the base-directory footer rides along on the `:` path too, so a
        // user-invoked skill's relative paths resolve like a model-loaded one's.
        assert!(
            out.contains("Base directory for this skill: /tmp/skills/audit"),
            "{out}"
        );
        // Not an invocation / unknown name → None (sent to the model as-is).
        assert!(expand_invocation(":nope", &[], &skills).is_none());
        assert!(expand_invocation("hello :audit", &[], &skills).is_none());
    }

    /// The tool that makes skills model-invocable: it resolves a name against
    /// the shared set and returns the body framed with its source, and always
    /// with the base directory — without which every relative path in the body
    /// resolves against the wrong directory.
    #[tokio::test]
    async fn skill_tool_returns_the_body_with_a_base_directory_footer() {
        use hrdr_tools::Tool;
        let skill = Skill {
            name: "pdf-fill".to_string(),
            description: "fill a pdf".to_string(),
            body: "Run scripts/fill.py.".to_string(),
            source: "~/.claude/skills".to_string(),
            base_dir: PathBuf::from("/home/me/.claude/skills/pdf-fill"),
            license: None,
            compatibility: None,
            metadata: BTreeMap::new(),
        };
        let tool = SkillTool {
            skills: Arc::new(Mutex::new(vec![skill])),
        };
        let ctx = hrdr_tools::ToolContext::new(std::env::temp_dir());

        let out = tool
            .execute(serde_json::json!({"name": "pdf-fill"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("Run scripts/fill.py."), "{out}");
        assert!(
            out.contains("Skill `pdf-fill` (source: ~/.claude/skills)"),
            "{out}"
        );
        assert!(
            out.contains("Base directory for this skill: /home/me/.claude/skills/pdf-fill"),
            "the footer is what makes `scripts/fill.py` resolvable: {out}"
        );

        // A leading `:` is accepted and the name matches case-insensitively.
        assert!(
            tool.execute(serde_json::json!({"name": ":PDF-Fill"}), &ctx)
                .await
                .is_ok()
        );
        // Read-only: a read-only sub-agent keeps it.
        assert!(tool.read_only());
    }

    /// An unknown name is an error that names what *is* available; a missing
    /// one is an error too, not a silent first-skill pick.
    #[tokio::test]
    async fn skill_tool_rejects_an_unknown_name_and_lists_the_known_ones() {
        use hrdr_tools::Tool;
        let tool = SkillTool {
            skills: Arc::new(Mutex::new(vec![Skill {
                name: "pdf-fill".to_string(),
                description: "fill a pdf".to_string(),
                body: "Body.".to_string(),
                source: "test".to_string(),
                base_dir: PathBuf::from("/tmp/skills/pdf-fill"),
                license: None,
                compatibility: None,
                metadata: BTreeMap::new(),
            }])),
        };
        let ctx = hrdr_tools::ToolContext::new(std::env::temp_dir());
        let err = tool
            .execute(serde_json::json!({"name": "pdf"}), &ctx)
            .await
            .unwrap_err();
        let shown = format!("{err:#}");
        assert!(shown.contains("unknown skill 'pdf'"), "{shown}");
        assert!(shown.contains("available: pdf-fill"), "{shown}");
        assert!(tool.execute(serde_json::json!({}), &ctx).await.is_err());
    }

    /// A root holding far more than `MAX_SKILLS` bundles yields a bounded set:
    /// ingestion stops at the aggregate file-count cap rather than reading every
    /// file.
    #[test]
    fn discovery_caps_the_file_count() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".hrdr/skills");
        for i in 0..(MAX_SKILLS + 20) {
            let name = format!("skill{i:04}");
            bundle(&root, &name, &skill_md(&name, "d", "Body."));
        }
        let found = discover_skills(dir.path(), crate::prompt::ProjectInstructions::Load);
        assert_eq!(found.skills.len(), MAX_SKILLS);
    }

    /// A root holding more bytes than `MAX_SKILLS_TOTAL_BYTES` yields a bounded
    /// set even while the file COUNT stays far under `MAX_SKILLS`: the two
    /// ceilings are independent, and this is the one a few big bundles hit.
    #[test]
    fn discovery_caps_the_total_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".hrdr/skills");
        // Each file at the per-file ceiling, so the aggregate budget buys exactly
        // this many before it is spent.
        let per_file = MAX_SKILL_FILE_BYTES as usize;
        let affordable = MAX_SKILLS_TOTAL_BYTES / per_file;
        for i in 0..(affordable + 6) {
            let name = format!("skill{i:04}");
            bundle(&root, &name, &skill_md_of_size(&name, per_file));
        }
        let found = discover_skills(dir.path(), crate::prompt::ProjectInstructions::Load);
        assert_eq!(found.skills.len(), affordable);
        assert!(
            affordable < MAX_SKILLS,
            "the byte cap is what bit here, not the file-count cap"
        );
    }

    /// A `SKILL.md` over `MAX_SKILL_FILE_BYTES` is skipped before it is read —
    /// and skipped silently, not reported as invalid: nothing about it was
    /// parsed, so there is no authoring mistake to name. The cap is a ceiling,
    /// so a file of exactly that size still loads.
    #[test]
    fn discovery_refuses_a_skill_file_over_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".hrdr/skills");
        let cap = MAX_SKILL_FILE_BYTES as usize;
        bundle(&root, "huge", &skill_md_of_size("huge", cap + 1));
        bundle(&root, "atcap", &skill_md_of_size("atcap", cap));

        let found = discover_skills(dir.path(), crate::prompt::ProjectInstructions::Load);
        assert!(
            !found.skills.iter().any(|s| s.name == "huge"),
            "{:?}",
            found.skills
        );
        assert!(found.invalid.is_empty(), "{:?}", found.invalid);
        assert!(
            found.skills.iter().any(|s| s.name == "atcap"),
            "a file at exactly the cap is within it: {:?}",
            found.skills
        );
    }

    /// A bundle whose name a higher-precedence root already holds is kept, not
    /// dropped — it can never run, and the `/commands` picker needs the row to
    /// say so.
    #[test]
    fn a_shadowed_duplicate_is_kept_for_the_picker() {
        let dir = tempfile::tempdir().unwrap();
        bundle(
            &dir.path().join(".hrdr/skills"),
            "ship",
            &skill_md("ship", "hrdr's ship", "hrdr wins"),
        );
        bundle(
            &dir.path().join(".claude/skills"),
            "ship",
            &skill_md("ship", "claude's ship", "claude loses"),
        );

        let found = discover_skills(dir.path(), crate::prompt::ProjectInstructions::Load);
        assert_eq!(found.skills.len(), 1);
        assert_eq!(found.skills[0].body, "hrdr wins");
        assert_eq!(found.shadowed.len(), 1, "{:?}", found.shadowed);
        assert_eq!(found.shadowed[0].name, "ship");
        assert_eq!(found.shadowed[0].body, "claude loses");
    }

    /// `$CODEX_HOME` moves codex's user root, and an empty value is no value.
    /// [`skill_dirs`] is tested rather than discovery because a real bundle
    /// planted under the shared sandbox `$HOME` would leak into every other
    /// test's discovery.
    ///
    /// `set_var` is process-global, so this holds [`ENV_LOCK`] and restores what
    /// it found.
    #[test]
    fn codex_home_overrides_the_default_codex_root() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let previous = std::env::var_os(CODEX_HOME);
        let home = crate::agents_dir::home_dir().expect("the test sandbox sets $HOME");
        let default_root = home.join(".codex").join("skills");
        let cwd = Path::new("/nonexistent-cwd");
        let dirs = || skill_dirs(cwd, crate::prompt::ProjectInstructions::Skip);

        // SAFETY: ENV_LOCK makes this the only thread touching CODEX_HOME, and
        // nothing else in the process reads it. Same for the three below.
        unsafe { std::env::remove_var(CODEX_HOME) };
        assert!(dirs().contains(&default_root), "{:?}", dirs());

        let elsewhere = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(CODEX_HOME, elsewhere.path()) };
        assert!(
            dirs().contains(&elsewhere.path().join("skills")),
            "{:?}",
            dirs()
        );
        assert!(
            !dirs().contains(&default_root),
            "the override replaces the default, it does not add to it: {:?}",
            dirs()
        );

        unsafe { std::env::set_var(CODEX_HOME, "") };
        assert!(
            dirs().contains(&default_root),
            "an empty value is not a location: {:?}",
            dirs()
        );

        match previous {
            Some(v) => unsafe { std::env::set_var(CODEX_HOME, v) },
            None => unsafe { std::env::remove_var(CODEX_HOME) },
        }
    }

    /// A `SKILL.md` big enough to blow the tool's output cap comes back
    /// truncated with the standard overflow pointer, and the rest is readable
    /// from the file it names — a procedure the model asked for by name is never
    /// silently cut short.
    #[tokio::test]
    async fn skill_tool_truncates_a_huge_body_and_spills_it_to_a_file() {
        use hrdr_tools::Tool;
        const LAST_STEP: &str = "the-final-step-marker";
        // Twice the cap: an 8-byte line repeated a quarter-of-the-cap times.
        let body = format!(
            "{}{LAST_STEP}\n",
            "padding\n".repeat(SKILL_OUTPUT_MAX_BYTES / 4)
        );
        let tool = SkillTool {
            skills: Arc::new(Mutex::new(vec![Skill {
                name: "long".to_string(),
                description: "a very long procedure".to_string(),
                body: body.clone(),
                source: "test".to_string(),
                base_dir: PathBuf::from("/tmp/skills/long"),
                license: None,
                compatibility: None,
                metadata: BTreeMap::new(),
            }])),
        };
        let ctx = hrdr_tools::ToolContext::new(std::env::temp_dir());

        let out = tool
            .execute(serde_json::json!({"name": "long"}), &ctx)
            .await
            .unwrap();
        assert!(
            out.len() < body.len(),
            "the body is far over the cap, so the output must be shorter than it: {} vs {}",
            out.len(),
            body.len()
        );
        assert!(
            !out.contains(LAST_STEP),
            "the tail is what was cut — otherwise nothing was truncated"
        );
        let saved = out
            .split("saved to ")
            .nth(1)
            .and_then(|rest| rest.split(" — ").next())
            .unwrap_or_else(|| panic!("no overflow pointer in the output:\n{out}"));
        let spilled = std::fs::read_to_string(saved).unwrap();
        assert!(
            spilled.contains(LAST_STEP),
            "the spill file holds the part that was cut"
        );
        assert!(
            spilled.contains("Base directory for this skill: /tmp/skills/long"),
            "…and the footer, so the spilled copy is the whole prompt"
        );
    }

    /// **A symlinked skill ROOT is walked; a symlink INSIDE one is not.**
    ///
    /// `discover_skills` builds its walk with `ignore`'s default
    /// `follow_links(false)`, but walkdir follows a symlinked *root* regardless
    /// (`follow_root_links` defaults on), and `WalkBuilder` leaves that default
    /// alone. So `~/.agents/skills -> ~/dotfiles/skills` — the dotfiles layout —
    /// does contribute its bundles, while a `shared -> …` link dropped inside a
    /// root contributes nothing. Both halves are asserted because the two look
    /// identical from outside and behave oppositely.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_is_walked_but_a_symlink_inside_one_is_not() {
        let dotfiles = tempfile::tempdir().unwrap();
        bundle(dotfiles.path(), "ship", &skill_md("ship", "d", "Body."));

        let linked_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(linked_root.path().join(".hrdr")).unwrap();
        std::os::unix::fs::symlink(dotfiles.path(), linked_root.path().join(".hrdr/skills"))
            .unwrap();
        let found = discover_skills(linked_root.path(), crate::prompt::ProjectInstructions::Load);
        assert!(
            found.skills.iter().any(|s| s.name == "ship"),
            "a symlinked root IS walked: {:?}",
            found.skills
        );

        let nested = tempfile::tempdir().unwrap();
        let root = nested.path().join(".hrdr/skills");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(dotfiles.path(), root.join("shared")).unwrap();
        let found = discover_skills(nested.path(), crate::prompt::ProjectInstructions::Load);
        assert!(
            found.skills.is_empty(),
            "a symlink below the root is NOT followed: {:?}",
            found.skills
        );
    }

    /// A symlink cycle inside a skill root must not hang discovery — a liveness
    /// guard, so the failure it catches is the run never returning.
    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_does_not_hang_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".hrdr/skills");
        bundle(&root, "ship", &skill_md("ship", "d", "Body."));
        std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();

        let found = discover_skills(dir.path(), crate::prompt::ProjectInstructions::Load);
        assert!(found.skills.iter().any(|s| s.name == "ship"));
    }
}

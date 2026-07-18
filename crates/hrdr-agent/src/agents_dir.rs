//! Discovery of sub-agent definitions from **Markdown files**, compatible with
//! Claude Code (`.claude/agents/`) and opencode (`.opencode/agent/`) layouts as
//! well as hrdr's own (`.hrdr/agents/`).
//!
//! Each file is Markdown with a YAML frontmatter block, parsed via
//! `serde_yaml_ng` (matching `hrdr-app`'s skill parser); the body is the
//! agent's system prompt. We map the flat frontmatter fields we understand
//! (name/description/model/tools/knobs) into [`FmValue`]s; a nested mapping
//! (e.g. opencode's per-tool boolean `tools:` map) has no representation
//! there, so that key is simply omitted — the rest of the file still loads.
//!
//! Frontmatter that fails to parse as YAML, or that parses to something other
//! than a mapping, fails that ONE file CLOSED: it is skipped rather than
//! loaded with defaults, because a frontmatter we can't read might carry
//! `read_only: true` or a `tools:` allow-list, and guessing "no frontmatter"
//! could silently drop those restrictions. `read_dir_profiles` reports this
//! with an `eprintln!` naming the file and keeps discovering the rest of the
//! directory — distinct from the legacy `provider:` key (see
//! [`parse_agent_file`]), which still hard-errors the whole discovery.
//!
//! Files are collected across all locations and **deduped by name** (case
//! -insensitive): the first source in precedence order wins (project before
//! user; hrdr → claude → opencode). The caller layers these over the built-in
//! agents, and `[[subagent]]` config over these.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::SubagentProfile;

/// Max bytes for a single agent profile file; files larger than this are skipped.
const MAX_AGENTS_FILE_BYTES: u64 = 64 * 1024; // 64 KiB

/// Discover agent-definition files across the Claude/opencode/hrdr locations,
/// relative to `cwd` for project scopes and the home/XDG dirs for user scopes.
/// Returns one profile per unique name (first source in precedence order wins).
///
/// Errors when a file still spells the identity as the old `provider:` +  `model:`
/// pair. An agent file is **configuration**, and the two keys could always
/// disagree — so, like config.toml, a stale one is refused rather than guessed at.
pub fn discover_agent_profiles(cwd: &Path) -> Result<Vec<SubagentProfile>> {
    let mut out: Vec<SubagentProfile> = Vec::new();
    for dir in agent_dirs(cwd) {
        for profile in read_dir_profiles(&dir)? {
            // First source wins: skip a name already registered.
            if out
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(&profile.name))
            {
                continue;
            }
            out.push(profile);
        }
    }
    Ok(out)
}

/// The agent directories to scan, in precedence order (highest first).
fn agent_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Project scopes (nearest / most specific) first.
    dirs.push(cwd.join(".hrdr").join("agents"));
    dirs.push(cwd.join(".claude").join("agents"));
    dirs.push(cwd.join(".opencode").join("agent"));
    // User scopes.
    if let Some(d) = crate::config_dir() {
        dirs.push(d.join("agents")); // ~/.config/hrdr/agents
    }
    if let Some(home) = home_dir() {
        dirs.push(home.join(".claude").join("agents"));
    }
    if let Ok(d) = hjkl_xdg::config_dir("opencode") {
        dirs.push(d.join("agent")); // ~/.config/opencode/agent
    }
    dirs
}

/// Home directory, cross-platform (`$HOME`, else `%USERPROFILE%`).
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Parse every `*.md` file in `dir` (non-recursive) into a profile. Missing or
/// unreadable directories yield nothing; a file with no usable content is skipped.
///
/// Two kinds of per-file parse failure are handled differently:
/// - Invalid/non-mapping YAML frontmatter ([`InvalidYamlFrontmatter`]) fails
///   CLOSED but only for that one file: it's skipped, with an `eprintln!`
///   naming the file and the YAML error, and discovery continues with the
///   rest of the directory.
/// - The dead `provider:` key (or any other parse error) is a hard error,
///   named by path, that fails the whole directory — that behavior is
///   intentional and covered by
///   [`a_provider_key_in_an_agent_file_is_an_error_naming_the_fix`](tests::a_provider_key_in_an_agent_file_is_an_error_naming_the_fix).
fn read_dir_profiles(dir: &Path) -> Result<Vec<SubagentProfile>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut profiles = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if path.metadata().map(|m| m.len()).unwrap_or(0) > MAX_AGENTS_FILE_BYTES {
            eprintln!("hrdr: skipping agent file {} (too large)", path.display());
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        match parse_agent_file(&text, stem) {
            Ok(Some(p)) => profiles.push(p),
            Ok(None) => {}
            Err(e) if e.is::<InvalidYamlFrontmatter>() => {
                eprintln!(
                    "hrdr: skipping agent file {} — invalid frontmatter: {e:#}",
                    path.display()
                );
            }
            Err(e) => return Err(legacy_error(&path, &e)),
        }
    }
    // Stable order within a directory (read_dir order is unspecified).
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(profiles)
}

/// An agent file's parse error, named by the file it came from.
fn legacy_error(path: &Path, err: &anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!("hrdr: {}: {err:#}", path.display())
}

/// Parse one agent file (`text`) into a profile, using `filename_stem` as the
/// fallback name. `Ok(None)` if there's no usable content (no name and no
/// body/prompt).
///
/// Errors on the old `provider:` key: the identity is ONE key now
/// (`model: provider://model`), and a file naming a provider beside a model can
/// name a pair that never agreed. The message says exactly what to write instead.
pub fn parse_agent_file(text: &str, filename_stem: &str) -> Result<Option<SubagentProfile>> {
    let (fm, body) = split_frontmatter(text)?;
    let body = body.trim();

    let name = fm
        .get("name")
        .map(|v| v.scalar())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| filename_stem.to_string());
    if name.is_empty() {
        return Ok(None);
    }

    // System prompt: the body, else an inline `prompt:` frontmatter value.
    let prompt = if !body.is_empty() {
        Some(body.to_string())
    } else {
        fm.get("prompt")
            .map(|v| v.scalar())
            .filter(|s| !s.is_empty())
    };

    let description = fm
        .get("description")
        .map(|v| v.scalar())
        .filter(|s| !s.is_empty());
    // The dead key. A provider beside a model is exactly the pair that could
    // disagree — an agent file names the whole identity in `model`, or nothing.
    if let Some(provider) = fm
        .get("provider")
        .map(|v| v.scalar())
        .filter(|s| !s.is_empty())
    {
        let model = fm
            .get("model")
            .map(|v| v.scalar())
            .filter(|s| !s.is_empty() && s != "inherit")
            .unwrap_or_else(|| "<model-id>".to_string());
        bail!(
            "agent '{name}' uses the old split provider/model keys.\n  replace:\n      \
             provider: {provider}\n      model: {model}\n  with:\n      \
             model: {provider}://{model}"
        );
    }
    // Claude's `inherit` = "the main agent's identity" = no spec at all.
    let model = fm
        .get("model")
        .map(|v| v.scalar())
        .filter(|s| !s.is_empty() && s != "inherit")
        .map(|s| s.parse::<crate::ModelSpec>())
        .transpose()
        .map_err(|e| anyhow::anyhow!("agent '{name}': model: {e}"))?;

    let temperature = fm.get("temperature").and_then(|v| v.scalar().parse().ok());
    let effort = fm
        .get("effort")
        .or_else(|| fm.get("reasoningEffort"))
        .map(|v| v.scalar())
        .filter(|s| !s.is_empty());
    // Claude `maxTurns`, opencode `steps`, hrdr `max_steps`.
    let max_steps = ["max_steps", "maxTurns", "steps"]
        .iter()
        .find_map(|k| fm.get(*k))
        .and_then(|v| v.scalar().parse().ok());

    let is_true = |v: &FmValue| matches!(v.scalar().as_str(), "true" | "yes" | "1");
    // `None` when the file doesn't mention the key at all — distinct from an
    // explicit `false` — so an `[[subagent]]` config overlay onto a same-named
    // discovered profile can tell "unset" from "set false" (see
    // `SubagentProfile::read_only`).
    let read_only = fm.get("read_only").map(is_true);
    // Only an allow-list form is honored (Claude/hrdr). opencode's boolean
    // `tools:` map is nested, so it parses to an empty list here and is ignored.
    let tools = fm.get("tools").map(|v| v.list()).filter(|l| !l.is_empty());
    let proactive = fm.get("proactive").map(is_true);

    Ok(Some(SubagentProfile {
        name,
        model,
        description,
        prompt,
        read_only,
        tools,
        temperature,
        effort,
        max_steps,
        proactive,
    }))
}

/// A frontmatter value, converted from a parsed [`serde_yaml_ng::Value`]: a
/// scalar, or a list. A YAML mapping value (e.g. opencode's per-tool boolean
/// `tools:` map) has no representation here — see [`fm_value_from_yaml`].
#[derive(Debug, Clone)]
enum FmValue {
    Scalar(String),
    List(Vec<String>),
}

impl FmValue {
    fn scalar(&self) -> String {
        match self {
            FmValue::Scalar(s) => s.clone(),
            FmValue::List(l) => l.join(", "),
        }
    }
    fn list(&self) -> Vec<String> {
        match self {
            FmValue::List(l) => l.clone(),
            FmValue::Scalar(s) if s.is_empty() => Vec::new(),
            // A scalar in list position may be CSV (`Read, Grep`, a plain YAML
            // scalar — commas are legal outside flow syntax) or one item.
            FmValue::Scalar(s) => s
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect(),
        }
    }
}

/// Split a leading `---` … `---` frontmatter fence off `text`, returning
/// `(frontmatter_text, body)`. Strips an optional leading BOM before looking
/// for the opening fence, and tolerates a CRLF line ending on the opening
/// fence (`---\r\n`) as well as a closing fence with trailing whitespace
/// (`--- `) and/or a CRLF ending. Returns `None` when there's no (properly
/// opened and terminated) fence — the caller then treats the *original*
/// input as the whole body.
///
/// Shared by `hrdr-agent`'s agent-file frontmatter (which further parses the
/// returned frontmatter text as YAML via [`split_frontmatter`]) and
/// `hrdr-app`'s skill files (which parse it as YAML too, via its own
/// `serde_yaml_ng` call) — the fence-splitting itself, including two
/// independently-fixed CRLF bugs, used to be duplicated between the two.
pub fn split_fence(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = trimmed.strip_prefix("---")?;
    // The opening fence must be its own line. Tolerate a CRLF line ending
    // (`---\r\n`): without this, a CRLF-authored file fails the `\n` match
    // and the ENTIRE file — including a `read_only: true` / `tools:`
    // allow-list — is returned as the body, loading the agent with no
    // restrictions and the raw YAML as its system prompt.
    let rest = rest.strip_prefix('\r').unwrap_or(rest);
    let rest = rest.strip_prefix('\n')?;
    // Find the closing fence line (`---` on its own line).
    let end = find_closing_fence(rest)?;
    let (fm_text, after) = rest.split_at(end);
    // Skip past the END of the closing fence LINE, not a literal `---\n`
    // prefix: a fence with trailing whitespace (`--- `) or a `\r` (`---\r\n`)
    // doesn't match `trim_start_matches("---").strip_prefix('\n')` exactly,
    // which silently discarded the whole body. `find_closing_fence` already
    // matched this line via `trim_end() == "---"`, so anything up to and
    // including its newline is the fence; everything after is the body.
    let body = match after.find('\n') {
        Some(nl) => &after[nl + 1..],
        None => "",
    };
    Some((fm_text, body))
}

/// Byte offset of the closing `---` fence line within `s` (the start of that
/// line), or `None` if unterminated.
fn find_closing_fence(s: &str) -> Option<usize> {
    let mut offset = 0;
    for line in s.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// Split `text` into (frontmatter map, body). A leading `---` … `---` fence is
/// the frontmatter, parsed as real YAML via `serde_yaml_ng`; without a fence
/// at all, the whole text is the body and the frontmatter is empty — that's
/// `split_fence` returning `None`, not a YAML error, so a plain prompt file
/// with no frontmatter still loads exactly as before.
///
/// Fails CLOSED on a fence that's present but broken: if the fence text isn't
/// valid YAML, or parses to something other than a mapping (a bare scalar or
/// list frontmatter isn't sensible either), this returns
/// `Err(InvalidYamlFrontmatter)` instead of silently treating the file as
/// having no frontmatter — a frontmatter we can't read might carry
/// `read_only: true` or a `tools:` allow-list, and loading with defaults
/// would drop those restrictions. Callers decide what "closed" means (see
/// [`read_dir_profiles`], which skips just this one file).
fn split_frontmatter(text: &str) -> Result<(std::collections::HashMap<String, FmValue>, &str)> {
    let Some((fm_text, body)) = split_fence(text) else {
        return Ok((std::collections::HashMap::new(), text));
    };
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(fm_text)
        .map_err(|e| InvalidYamlFrontmatter(format!("invalid YAML: {e}")))?;
    let serde_yaml_ng::Value::Mapping(mapping) = value else {
        return Err(InvalidYamlFrontmatter("frontmatter is not a YAML mapping".to_string()).into());
    };
    let mut map = std::collections::HashMap::new();
    for (k, v) in mapping {
        // A non-string key can't name a frontmatter field we understand.
        if let (Some(key), Some(value)) = (k.as_str(), fm_value_from_yaml(v)) {
            map.insert(key.to_string(), value);
        }
    }
    Ok((map, body))
}

/// The fence's text parsed as something other than valid YAML, or valid YAML
/// that isn't a mapping. Distinguished (via `anyhow::Error::is`) from every
/// other [`parse_agent_file`] failure — chiefly the legacy `provider:` bail —
/// so [`read_dir_profiles`] can fail CLOSED on just the one file (skip +
/// `eprintln!`) while still hard-erroring the whole discovery for everything
/// else.
#[derive(Debug)]
struct InvalidYamlFrontmatter(String);

impl std::fmt::Display for InvalidYamlFrontmatter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for InvalidYamlFrontmatter {}

/// Convert one YAML mapping value into an [`FmValue`], matching what a
/// hand-authored frontmatter field means:
/// - A string scalar → `Scalar`, trimmed.
/// - A number/bool → `Scalar` of its string form (`read_only: true` → `"true"`,
///   still matched by the `is_true` check downstream; `temperature: 0.7`
///   still parses downstream).
/// - `key:` with nothing (YAML null) → `Scalar("")`, which the field mapping
///   in `parse_agent_file` filters out via `.filter(|s| !s.is_empty())`, same
///   as today.
/// - A sequence → `List` of its scalar elements, stringified the same way;
///   non-scalar elements (nested sequences/mappings) are skipped.
/// - A mapping (e.g. opencode's nested boolean `tools:` map) → `None`, so the
///   key is omitted entirely — nested maps aren't represented, matching
///   today's behavior. The file still loads; only that one key is dropped.
fn fm_value_from_yaml(v: serde_yaml_ng::Value) -> Option<FmValue> {
    match v {
        serde_yaml_ng::Value::String(s) => Some(FmValue::Scalar(s.trim().to_string())),
        serde_yaml_ng::Value::Number(n) => Some(FmValue::Scalar(n.to_string())),
        serde_yaml_ng::Value::Bool(b) => Some(FmValue::Scalar(b.to_string())),
        serde_yaml_ng::Value::Null => Some(FmValue::Scalar(String::new())),
        serde_yaml_ng::Value::Sequence(seq) => Some(FmValue::List(
            seq.iter().filter_map(scalar_element_to_string).collect(),
        )),
        serde_yaml_ng::Value::Mapping(_) => None,
        _ => None,
    }
}

/// Stringify one element of a YAML sequence for [`FmValue::List`]: only
/// string/number/bool scalars are meaningful list items (a `tools:` list is
/// always these); anything else — including a bare `null` item — is skipped
/// rather than stringified into a meaningless entry.
fn scalar_element_to_string(v: &serde_yaml_ng::Value) -> Option<String> {
    match v {
        serde_yaml_ng::Value::String(s) => Some(s.trim().to_string()),
        serde_yaml_ng::Value::Number(n) => Some(n.to_string()),
        serde_yaml_ng::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a file that must parse, and must carry a profile.
    fn parse(text: &str, stem: &str) -> SubagentProfile {
        parse_agent_file(text, stem)
            .expect("parses")
            .expect("a profile")
    }

    fn spec(s: &str) -> crate::ModelSpec {
        s.parse().expect("a valid model spec")
    }

    #[test]
    fn parses_claude_style_agent() {
        let text = "---\n\
            name: code-reviewer\n\
            description: Reviews code for quality\n\
            model: sonnet\n\
            tools: Read, Grep, Bash\n\
            ---\n\
            You are a careful code reviewer.\n";
        let p = parse(text, "fallback");
        assert_eq!(p.name, "code-reviewer");
        assert_eq!(p.description.as_deref(), Some("Reviews code for quality"));
        assert_eq!(p.model, Some(spec("sonnet")));
        assert_eq!(
            p.tools.as_deref(),
            Some(&["Read".into(), "Grep".into(), "Bash".into()][..])
        );
        assert_eq!(
            p.prompt.as_deref(),
            Some("You are a careful code reviewer.")
        );
    }

    #[test]
    fn parses_block_list_and_knobs() {
        let text = "---\n\
            description: Planner\n\
            temperature: 0.2\n\
            reasoningEffort: high\n\
            steps: 25\n\
            tools:\n\
            \x20 - Read\n\
            \x20 - Grep\n\
            ---\n\
            Body prompt.\n";
        let p = parse(text, "planner");
        assert_eq!(p.name, "planner"); // from filename
        assert_eq!(p.temperature, Some(0.2));
        assert_eq!(p.effort.as_deref(), Some("high"));
        assert_eq!(p.max_steps, Some(25));
        assert_eq!(
            p.tools.as_deref(),
            Some(&["Read".into(), "Grep".into()][..])
        );
    }

    #[test]
    fn ignores_nested_map_from_opencode_tools() {
        // opencode's boolean tools map is nested; we don't mistake its keys for
        // top-level frontmatter, and the empty list is dropped (tools = None).
        let text = "---\n\
            description: Build agent\n\
            mode: subagent\n\
            tools:\n\
            \x20 write: false\n\
            \x20 edit: false\n\
            model: anthropic/claude-sonnet-4\n\
            ---\n\
            Do the thing.\n";
        let p = parse(text, "build");
        assert_eq!(p.description.as_deref(), Some("Build agent"));
        // A bare slashed id is a MODEL, not a provider — `ModelSpec` splits on `://`
        // and nothing else, so opencode's `anthropic/claude-sonnet-4` still names a
        // model on the provider in force.
        assert_eq!(p.model, Some(spec("anthropic/claude-sonnet-4")));
        assert!(matches!(p.model, Some(crate::ModelSpec::ModelOnly(_))));
        assert!(p.tools.is_none(), "nested bool-map must not become a list");
    }

    #[test]
    fn parses_proactive_flag() {
        let text = "---\nname: reviewer\nproactive: true\n---\nreview stuff\n";
        assert!(parse(text, "x").is_proactive());
        let text = "---\nname: reviewer\n---\nreview stuff\n";
        assert!(!parse(text, "x").is_proactive());
        // Unmentioned means unset, not "explicitly false" — significant for the
        // field-level merge in `resolve_agent_profiles`.
        assert_eq!(parse(text, "x").proactive, None);
    }

    #[test]
    fn claude_inherit_model_is_treated_as_default() {
        let text = "---\nname: x\nmodel: inherit\n---\nbody\n";
        let p = parse(text, "x");
        assert!(p.model.is_none());
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let p = parse("Just a system prompt.", "helper");
        assert_eq!(p.name, "helper");
        assert_eq!(p.prompt.as_deref(), Some("Just a system prompt."));
        assert!(p.description.is_none());
    }

    #[test]
    fn discovery_dedupes_by_name_across_locations() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        // Same agent name in both project .claude and project .opencode.
        let claude = cwd.join(".claude").join("agents");
        let opencode = cwd.join(".opencode").join("agent");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(&opencode).unwrap();
        std::fs::write(
            claude.join("reviewer.md"),
            "---\nname: reviewer\ndescription: from claude\n---\nclaude body\n",
        )
        .unwrap();
        std::fs::write(
            opencode.join("reviewer.md"),
            "---\nname: reviewer\ndescription: from opencode\n---\nopencode body\n",
        )
        .unwrap();

        let found = discover_agent_profiles(cwd).unwrap();
        let revs: Vec<&SubagentProfile> = found.iter().filter(|p| p.name == "reviewer").collect();
        assert_eq!(revs.len(), 1, "same name registered once");
        // .claude precedes .opencode in the precedence order → it wins.
        assert_eq!(revs[0].description.as_deref(), Some("from claude"));
    }

    /// An agent file is CONFIG: the identity is one key, and a file still naming a
    /// provider beside a model is refused — with the file, the two lines it wrote,
    /// and the single line that replaces them.
    #[test]
    fn a_provider_key_in_an_agent_file_is_an_error_naming_the_fix() {
        let text =
            "---\nname: builder\nprovider: openrouter\nmodel: deepseek/deepseek-chat\n---\nbuild\n";
        let err = parse_agent_file(text, "builder")
            .expect_err("the old split keys are refused")
            .to_string();
        assert!(err.contains("old split provider/model keys"), "{err}");
        assert!(err.contains("provider: openrouter"), "{err}");
        assert!(
            err.contains("model: openrouter://deepseek/deepseek-chat"),
            "the fix is spelled out: {err}"
        );

        // …and the file it came from is named, so it can be found and fixed.
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(".hrdr").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("builder.md"), text).unwrap();
        let err = discover_agent_profiles(dir.path())
            .expect_err("discovery refuses it too")
            .to_string();
        assert!(err.contains("builder.md"), "names the file: {err}");
        assert!(
            err.contains("model: openrouter://deepseek/deepseek-chat"),
            "{err}"
        );
    }

    /// `model: inherit` still means "the main agent's identity" — no spec at all.
    #[test]
    fn a_model_only_file_needs_no_provider_at_all() {
        let p = parse("---\nname: x\nmodel: zen://kimi-k2\n---\nbody\n", "x");
        assert_eq!(p.model, Some(spec("zen://kimi-k2")));
        assert!(matches!(p.model, Some(crate::ModelSpec::Full(_))));
    }

    /// Security regression: a CRLF-authored agent file (`---\r\n`) must still
    /// have its frontmatter parsed — including `read_only` and the `tools`
    /// allow-list. Before the fix, `split_frontmatter` required `\n`
    /// immediately after the opening `---`, so a `\r` there made the whole
    /// file (raw YAML included) fall through as an unrestricted body, loading
    /// the agent with NO tool restrictions.
    #[test]
    fn crlf_frontmatter_still_restricts_the_agent() {
        let text = "---\r\nname: locked-down\r\nread_only: true\r\ntools: Read, Grep\r\n---\r\nBe careful.\r\n";
        let p = parse(text, "fallback");
        assert_eq!(p.name, "locked-down");
        assert!(p.is_read_only(), "read_only must survive CRLF frontmatter");
        assert_eq!(
            p.tools.as_deref(),
            Some(&["Read".into(), "Grep".into()][..]),
            "the tools allow-list must survive CRLF frontmatter"
        );
        assert_eq!(p.prompt.as_deref(), Some("Be careful."));
    }

    /// A closing fence with trailing whitespace (`--- `) must not silently
    /// discard the body: extraction skips to the end of the fence LINE rather
    /// than prefix-stripping the exact bytes `---\n`.
    #[test]
    fn closing_fence_trailing_whitespace_keeps_the_body() {
        let text = "---\nname: x\n--- \nThe system prompt.\n";
        let p = parse(text, "fallback");
        assert_eq!(p.prompt.as_deref(), Some("The system prompt."));
    }

    /// A closing fence written as `---\r\n` (CRLF) must not silently discard
    /// the body either — same body-extraction fix as the trailing-whitespace
    /// case above.
    #[test]
    fn closing_fence_crlf_keeps_the_body() {
        let text = "---\r\nname: x\r\n---\r\nThe system prompt.\r\n";
        let p = parse(text, "fallback");
        assert_eq!(p.prompt.as_deref(), Some("The system prompt."));
    }

    /// YAML block scalars (`description: |`, `prompt: >`) must not collapse
    /// to the literal punctuation `"|"` / `">"` with the indented block that
    /// follows silently dropped.
    #[test]
    fn block_scalars_are_not_literal_punctuation() {
        let text = "---\n\
            description: |\n\
            \x20 Line one.\n\
            \x20 Line two.\n\
            prompt: >\n\
            \x20 Folded one.\n\
            \x20 Folded two.\n\
            name: x\n\
            ---\n\
            body\n";
        let p = parse(text, "fallback");
        assert_eq!(
            p.description.as_deref(),
            Some("Line one.\nLine two."),
            "literal block scalar keeps its newlines"
        );
        // The body is non-empty, so `prompt:` (frontmatter) is not surfaced as
        // the profile's prompt — but it must still parse to the folded value
        // rather than the literal ">", which we check via the parsing seam
        // directly (real YAML, via `serde_yaml_ng`, handles this for free).
        let (map, _) =
            split_frontmatter("---\nprompt: >\n  Folded one.\n  Folded two.\n---\nbody\n").unwrap();
        assert_eq!(
            map.get("prompt").map(FmValue::scalar).as_deref(),
            Some("Folded one. Folded two."),
            "folded block scalar joins lines with spaces, not literal '>'"
        );
    }

    #[test]
    fn lone_block_scalar_marker_with_no_continuation_is_empty() {
        // A literal block scalar immediately followed by a same-indent key has
        // no content lines — valid YAML, value is the empty string, not the
        // literal `|` marker.
        let (map, _) = split_frontmatter("---\ndescription: |\nname: x\n---\nbody\n").unwrap();
        assert_eq!(
            map.get("description").map(FmValue::scalar).as_deref(),
            Some(""),
            "a block scalar with no indented continuation is empty, not '|'"
        );
    }

    /// `split_fence` is the shared helper `hrdr-app`'s skill parser also
    /// calls: exercise its CRLF opening fence, trailing-whitespace closing
    /// fence, body extraction, and no-fence `None` directly.
    #[test]
    fn split_fence_extracts_frontmatter_and_body() {
        // CRLF opening fence.
        let (fm, body) = split_fence("---\r\nname: x\r\n---\r\nbody text\r\n").unwrap();
        assert_eq!(fm, "name: x\r\n");
        assert_eq!(body, "body text\r\n");

        // Closing fence with trailing whitespace.
        let (fm, body) = split_fence("---\nname: x\n--- \nbody text\n").unwrap();
        assert_eq!(fm, "name: x\n");
        assert_eq!(body, "body text\n");

        // No opening fence at all → None.
        assert!(split_fence("no fence here").is_none());

        // Opening fence with no closing fence → None (unterminated).
        assert!(split_fence("---\nname: x\nno closing fence\n").is_none());
    }

    /// The prettier form: `description:` with nothing on that line, followed
    /// by an indented continuation line — valid YAML (folded into one plain
    /// string) that the old flat `key: value` scanner never noticed at all,
    /// since it only matched an inline value.
    #[test]
    fn plain_continuation_scalar_description_parses() {
        let text =
            "---\nname: x\ndescription:\n  a longer description on its own line\n---\nbody\n";
        let p = parse(text, "fallback");
        assert_eq!(
            p.description.as_deref(),
            Some("a longer description on its own line")
        );
    }

    /// Fail-closed: an agent file whose frontmatter carries `read_only: true`
    /// and a `tools:` allow-list, but also a tab-indented line (illegal YAML
    /// indentation, so the fence fails to parse), must NOT produce a profile
    /// at all — loading it with defaults instead of skipping it would
    /// silently drop the very restrictions it was trying to set. A
    /// well-formed sibling file in the same directory still loads: one
    /// broken file doesn't take down the whole directory.
    #[test]
    fn fail_closed_on_invalid_yaml_frontmatter_skips_just_that_file() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(".hrdr").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("broken.md"),
            "---\nname: broken\nread_only: true\ntools: Read, Grep\n\tbad: tab-indented\n---\nBe careful.\n",
        )
        .unwrap();
        std::fs::write(
            agents.join("fine.md"),
            "---\nname: fine\ndescription: a working agent\n---\nDo the thing.\n",
        )
        .unwrap();

        let found = discover_agent_profiles(dir.path()).unwrap();
        assert!(
            !found.iter().any(|p| p.name == "broken"),
            "invalid YAML frontmatter must not produce a profile, even with read_only/tools set"
        );
        let fine = found.iter().find(|p| p.name == "fine").unwrap();
        assert_eq!(fine.description.as_deref(), Some("a working agent"));
    }
}

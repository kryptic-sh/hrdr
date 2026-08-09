//! Commands: reusable prompt templates, invocable by the **user** with a `:`
//! prefix (`:name args…`) and by the **model** through the `command` tool.
//!
//! A command is a Markdown file — optional YAML frontmatter (`name:`,
//! `description:`, `args:`, `model_invocable:`), body = the prompt. On invocation the body is sent
//! to the model with `$ARGUMENTS` filled from the text after the command name: a
//! command that declares `args:` takes just the first token as its argument and
//! appends any trailing text as extra context, while a command without `args:`
//! takes the whole remainder (see [`expand_command`]). Discovery mirrors the
//! sub-agent files: project dirs first, then user dirs, hrdr → Claude Code →
//! opencode conventions, then hrdr's own built-in commands (`:deps`, `:cli`,
//! `:work`, `:release`, `:review`, `:audit`, `:fix`, `:todo`, `:test`,
//! `:plan`, `:tidy`, `:perf`, `:sweep`, …) last — deduped by name (first source
//! wins), so a user or project file always overrides a built-in of the same
//! name.
//!
//! Command directories are walked **recursively** and a nested file is
//! namespaced by its path: `.hrdr/commands/git/commit.md` is `:git/commit`.
//! That is opencode's naming, and `:` and `.` are accepted as spellings of the
//! separator too (see [`command_match_key`]), so `:git:commit` and
//! `:git.commit` reach the same file.
//!
//! This lives in `hrdr-agent` rather than in a frontend because the model can
//! invoke a command: the agent lists what is available in its system prompt
//! (`prompt::commands_section`) and loads a body on demand through the `command`
//! tool. Frontend-only concerns — the `:`-completion popup and the `/commands`
//! picker filter — stay in `hrdr-app` on top of this.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// The commands hrdr ships with, baked into the binary: the content lives in
// `src/templates/commands/*.md`, one file per command, and `build.rs` emits the
// registry (`COMMAND_FILES` below) at build time — adding a built-in command is
// adding a Markdown file, with no Rust wiring to touch. Keep the prompt text
// in Markdown (reviewable, diffable, editable without touching Rust) and this
// file to parsing/wiring only.
include!(concat!(env!("OUT_DIR"), "/builtin_commands.rs"));

/// Max bytes for a single command file; files larger than this are skipped.
const MAX_COMMAND_FILE_BYTES: u64 = 64 * 1024;

/// Aggregate ceilings on command ingestion across ALL command dirs combined: at
/// most this many command files read, and at most this many total bytes. A real
/// setup has a handful of small command Markdown files, so these ceilings are
/// far beyond anything genuine — the cap only stops a hostile or accidental
/// directory full of files from making hrdr read unbounded bytes on every `:`
/// input and command listing. Once either is hit we stop reading and warn; the
/// built-ins are always appended regardless.
const MAX_COMMANDS: usize = 256;
const MAX_COMMANDS_TOTAL_BYTES: usize = 4 * 1024 * 1024;

/// How deep a command directory is walked. Namespacing is a shallow habit
/// (`git/commit`, `db/migrate`), so this only bounds the traversal a hostile or
/// accidental deep tree would otherwise cost on every `:` keystroke.
const COMMAND_MAX_DEPTH: usize = 8;

/// One discovered command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Invocation name (`:name`) — frontmatter `name:`, else the file's path
    /// relative to its command directory with `.md` stripped, so a nested
    /// `git/commit.md` is `git/commit`. A frontmatter `name:` is taken whole:
    /// it is not prefixed with the directory it was found in.
    pub name: String,
    /// One-line summary for the completion popup / `/commands` listing, and the
    /// only thing about a command that the system prompt spends tokens on.
    pub description: String,
    /// The prompt template (the file body).
    pub body: String,
    /// Where it came from, for the `/commands` listing (home-shortened dir).
    pub source: String,
    /// Candidate argument values (frontmatter `args:`, comma-separated or
    /// `[a, b]`), offered by the completion popup after `:name `.
    pub args: Vec<String>,
    /// The model may load this itself (frontmatter `model_invocable:`, default
    /// `true`). `false` keeps it out of the prompt listing and makes the `command`
    /// tool refuse it: the user's `:name` stays the only way in — for a
    /// procedure whose last step is outward-facing and hard to reverse, deciding
    /// to run it is the user's call, not the model's.
    pub model_invocable: bool,
}

/// The command directories to scan, in precedence order (highest first).
/// opencode accepts both `command` and `commands`, so both are scanned for it.
///
/// `project` decides whether the working tree's own directories are scanned at
/// all — see [`ProjectInstructions`](crate::prompt::ProjectInstructions). They are
/// the worst of the instruction surfaces to load from an untrusted repo: they are
/// discovered **before** the built-ins and shadow them by name, with
/// `model_invocable` defaulting true, so a repo shipping `.hrdr/commands/commit.md`
/// replaces the vetted `:commit` outright.
fn command_dirs(cwd: &Path, project: crate::prompt::ProjectInstructions) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Project scopes (nearest / most specific) first.
    if project == crate::prompt::ProjectInstructions::Load {
        dirs.push(cwd.join(".hrdr").join("commands"));
        dirs.push(cwd.join(".claude").join("commands"));
        dirs.push(cwd.join(".opencode").join("command"));
        dirs.push(cwd.join(".opencode").join("commands"));
    }
    // User scopes.
    if let Some(d) = crate::config_dir() {
        dirs.push(d.join("commands")); // ~/.config/hrdr/commands
    }
    if let Some(home) = crate::agents_dir::home_dir() {
        dirs.push(home.join(".claude").join("commands"));
    }
    if let Ok(d) = hjkl_xdg::config_dir("opencode") {
        dirs.push(d.join("command")); // ~/.config/opencode/command
        dirs.push(d.join("commands"));
    }
    dirs
}

/// The name a command file gets from where it sits: its path relative to the
/// command directory, `.md` stripped, components joined with `/` — so
/// `<dir>/git/commit.md` is `git/commit` and `<dir>/commit.md` is `commit`.
/// This is opencode's naming (it globs `{command,commands}/**/*.md` and names
/// each by the same relative path). `None` when `path` is outside `dir` or any
/// component is not UTF-8.
fn namespaced_name(dir: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(dir).ok()?.with_extension("");
    let parts: Vec<&str> = rel
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<Vec<&str>>>()?;
    let name = parts.join("/");
    (!name.is_empty()).then_some(name)
}

/// The key two command names are compared by. `/` is the canonical namespace
/// separator, but `:` and `.` are accepted spellings of it — a user reaching
/// for `:git:commit` or `:git.commit` means the same file as `:git/commit` —
/// and, as everywhere else here, names match case-insensitively.
pub fn command_match_key(name: &str) -> String {
    name.replace([':', '.'], "/").to_ascii_lowercase()
}

/// Discover command files across the hrdr/Claude/opencode locations, relative to
/// `cwd` for project scopes, plus hrdr's built-in commands. Each directory is
/// walked recursively and a nested file is namespaced by its path
/// ([`namespaced_name`]). One command per unique name — compared by
/// [`command_match_key`], so `git/commit` and `git:commit` are the same command,
/// not two — and the first source in precedence order wins: the built-ins are
/// appended last, so any user or project file of the same name (e.g. a project's
/// own `.hrdr/commands/commit.md`) is discovered first and shadows it.
pub fn discover_commands(cwd: &Path, project: crate::prompt::ProjectInstructions) -> Vec<Command> {
    let mut out: Vec<Command> = Vec::new();
    // Aggregate budget across ALL command dirs combined. Dirs are scanned in
    // precedence order (project before user), so exhausting the budget drops
    // the least-specific files first.
    let mut file_count: usize = 0;
    let mut total_bytes: usize = 0;
    let mut truncated = false;
    for dir in command_dirs(cwd, project) {
        if !dir.is_dir() {
            continue;
        }
        let mut found: Vec<Command> = Vec::new();
        // A command dir is itself hidden (`.hrdr/`, `.claude/`) and is often
        // gitignored, so none of `ignore`'s standard filters belong here — they
        // would silently drop the very files being looked for. Symlinks are not
        // followed (the builder's default), which is also what keeps a symlink
        // loop from turning this walk into a hang.
        for entry in ignore::WalkBuilder::new(&dir)
            .standard_filters(false)
            .max_depth(Some(COMMAND_MAX_DEPTH))
            .sort_by_file_path(Path::cmp)
            .build()
            .flatten()
        {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if path.metadata().map(|m| m.len()).unwrap_or(0) > MAX_COMMAND_FILE_BYTES {
                continue;
            }
            // Per file visited, so a deep tree cannot spend more than the
            // aggregate budget however it is shaped.
            if file_count >= MAX_COMMANDS || total_bytes >= MAX_COMMANDS_TOTAL_BYTES {
                truncated = true;
                break;
            }
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            file_count += 1;
            total_bytes = total_bytes.saturating_add(text.len());
            let Some(name) = namespaced_name(&dir, path) else {
                continue;
            };
            if let Some(command) = parse_command_file(&text, &name, &crate::display_dir(&dir)) {
                found.push(command);
            }
        }
        // Stable order within a directory, by the name each is invoked under
        // (which frontmatter may have changed from the path-derived one).
        found.sort_by(|a, b| a.name.cmp(&b.name));
        for command in found {
            let key = command_match_key(&command.name);
            if !out.iter().any(|c| command_match_key(&c.name) == key) {
                out.push(command);
            }
        }
        // Merge this dir's finds before stopping, so nothing already read is lost.
        if truncated {
            // Silent on purpose: `discover_commands` runs inside the TUI (on every
            // cwd change and `:`-completion), so writing to stderr here would
            // corrupt the display. The cap is a defensive ceiling (`MAX_COMMANDS` /
            // `MAX_COMMANDS_TOTAL_BYTES`) no real setup reaches, so there is nothing
            // actionable to say.
            break;
        }
    }
    for command in builtin_commands() {
        let key = command_match_key(&command.name);
        if !out.iter().any(|c| command_match_key(&c.name) == key) {
            out.push(command);
        }
    }
    out
}

/// hrdr's built-in commands — `:commit`, `:release`, `:review`, `:audit`,
/// `:fix`, `:todo`, `:test`, `:plan`, `:tidy`, `:perf` — parsed from
/// the Markdown templates baked into the binary at compile time. Always one
/// entry per template (each is a checked-in, non-empty file, so parsing cannot
/// fail); sorted by name like a scanned directory's entries are, so their
/// relative order matches wherever they'd sit if they were plain files on
/// disk.
pub fn builtin_commands() -> Vec<Command> {
    let mut commands: Vec<Command> = COMMAND_FILES
        .iter()
        .filter_map(|(stem, text)| parse_command_file(text, stem, "built-in"))
        .collect();
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

/// Parse one command file: optional YAML frontmatter (a leading `---` … `---`
/// fence containing `name:` / `description:` / `args:` / `model_invocable:`),
/// body = the prompt. `None` when the body is empty.
///
/// `derived_name` is the name the file's location gives it (see
/// [`namespaced_name`]); a frontmatter `name:` replaces it outright, namespace
/// and all.
pub fn parse_command_file(text: &str, derived_name: &str, source: &str) -> Option<Command> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (fm, body) = match crate::split_fence(text) {
        Some((fm, body)) => (parse_frontmatter(fm), body),
        None => (Frontmatter::default(), text),
    };
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    Some(Command {
        name: fm.name.unwrap_or_else(|| derived_name.to_string()),
        description: fm.description.unwrap_or_default(),
        body: body.to_string(),
        source: source.to_string(),
        args: fm.args,
        model_invocable: fm.model_invocable,
    })
}

/// A command file's frontmatter fields, all optional.
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    args: Vec<String>,
    /// `model_invocable: false` keeps a command out of the prompt listing and
    /// makes the `command` tool refuse it — user-invocable only.
    model_invocable: bool,
}

impl Default for Frontmatter {
    fn default() -> Self {
        // Opt-out, not opt-in: an unmarked command is a procedure the user wants
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
/// failing the whole command: `split_fence` has already stripped the fence off
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
    // leaves the command model-invocable: failing open here costs a menu entry the
    // author may not have wanted, while failing closed would silently hide a
    // command and look like the feature is broken.
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
/// `name`/`description`/a single `args` element, nor for a `tools:` list item.
pub(crate) fn scalar_to_string(v: &serde_yaml_ng::Value) -> Option<String> {
    match v {
        serde_yaml_ng::Value::String(s) => Some(s.trim().to_string()),
        serde_yaml_ng::Value::Number(n) => Some(n.to_string()),
        serde_yaml_ng::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// If `input` invokes a command (`:name args…`, matched through
/// [`command_match_key`] — case-insensitively, and with `/`, `:` or `.` for the
/// namespace separator), return the prompt to send. `None` when the input isn't
/// a `:` invocation or names no known command (it then goes to the model as-is).
///
/// How the text after the name is used depends on whether the command declares
/// `args:`:
/// - A command **with** `args:` takes a single positional argument — the first
///   whitespace-delimited token. That token fills `$ARGUMENTS`, and anything
///   after it is extra free-form context appended to the body on its own line.
///   So `:audit high focus on the parser` runs the audit at depth `high` with
///   "focus on the parser" appended as guidance.
/// - A command **without** `args:` treats the whole remainder as `$ARGUMENTS`
///   (or, when the body has no placeholder, appends it) — free-form input like
///   a pasted error or a commit scope isn't split on the first space.
pub fn expand_command(input: &str, commands: &[Command]) -> Option<String> {
    let (name, after_name) = split_invocation(input)?;
    let key = command_match_key(name);
    let command = commands
        .iter()
        .find(|c| command_match_key(&c.name) == key)?;
    Some(expand_body(command, after_name))
}

/// Split a `:name rest…` invocation into its name and the trimmed text after it.
/// `None` when `input` is not a `:` invocation (or names nothing at all), which
/// is how a plain message reaches the model untouched.
///
/// Shared with [`crate::skills`]: commands and skills answer the same `:name`
/// namespace, so they must agree on where the name ends.
pub(crate) fn split_invocation(input: &str) -> Option<(&str, &str)> {
    let rest = input.trim_start().strip_prefix(':')?;
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next().filter(|n| !n.is_empty())?;
    Some((name, parts.next().unwrap_or("").trim()))
}

/// Fill `command`'s body from `arguments` — the shared half of [`expand_command`]
/// and the `command` tool, so a model-invoked command and a `:`-invoked one expand
/// byte-identically. `arguments` is everything after the command name.
///
/// A declared-`args:` command consumes only its first token as the argument; the
/// rest is appended. A command without `args:` takes the whole remainder.
pub fn expand_body(command: &Command, arguments: &str) -> String {
    let arguments = arguments.trim();
    let (arg, extra) = if command.args.is_empty() {
        (arguments, "")
    } else {
        let mut split = arguments.splitn(2, char::is_whitespace);
        (
            split.next().unwrap_or(""),
            split.next().unwrap_or("").trim(),
        )
    };

    let mut prompt = if command.body.contains("$ARGUMENTS") {
        command.body.replace("$ARGUMENTS", arg)
    } else if arg.is_empty() {
        command.body.clone()
    } else {
        format!("{}\n\n{arg}", command.body)
    };
    if !extra.is_empty() {
        prompt = format!("{prompt}\n\n{extra}");
    }
    prompt
}

/// The live command set, shared between the agent — which re-discovers it
/// whenever the cwd changes, so the prompt listing and the tool never disagree
/// — and [`CommandTool`].
pub(crate) type SharedCommands = Arc<Mutex<Vec<Command>>>;

/// Cap on the bytes one `command` call returns. Discovery already refuses a command
/// file over [`MAX_COMMAND_FILE_BYTES`], so this only bounds a pathological one; it
/// is deliberately far above `ctx.max_output` because a command body is a
/// *procedure the model just asked for by name*, and half a procedure is worse
/// than the tokens — the model would follow it anyway. Overflow still spills to
/// a file it can `read`, like every other tool's.
const COMMAND_OUTPUT_MAX_BYTES: usize = 24 * 1024;

/// `command` — load one command's instructions by name. The names and one-line
/// descriptions are already in the system prompt (`prompt::commands_section`), so
/// this tool is the "now give me the body" half: it costs nothing until the
/// model decides a command applies.
///
/// Read-only: it reads files discovery already read and returns text. What the
/// *body* then asks for is bounded by the agent's own tool set, exactly as it is
/// when a user types `:name`.
pub(crate) struct CommandTool {
    pub(crate) commands: SharedCommands,
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for CommandTool {
    fn name(&self) -> &'static str {
        "command"
    }

    fn description(&self) -> &'static str {
        "Load a command — a reusable procedure the user, this project, or hrdr itself wrote for a \
         recurring task (committing, releasing, reviewing, auditing…). The available commands are \
         listed by name and one-line description in your system prompt under `Commands`; this \
         returns the named one's full instructions. \
         Call it BEFORE doing the work whenever the task matches a command's description — the \
         command is how the user wants that job done, so following it beats improvising. \
         `arguments` is the free-form text the command's `$ARGUMENTS` placeholder takes (a depth, a \
         scope, a pasted error); omit it when the command needs none. \
         Read-only: it returns instructions and changes nothing. Follow them with the tools you \
         already have."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Command name, as listed under `Commands` in your system prompt (a leading `:` is accepted). A namespaced name may be written `git/commit`, `git:commit` or `git.commit` — all three name the same command."
                },
                "arguments": {
                    "type": "string",
                    "description": "Optional: text filling the command's $ARGUMENTS placeholder (e.g. `high` for an audit depth)."
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
            .ok_or_else(|| anyhow::anyhow!("command: `name` is required (a command name)"))?;
        let arguments = args
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let commands = self
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let key = command_match_key(name);
        let Some(command) = commands.iter().find(|c| command_match_key(&c.name) == key) else {
            anyhow::bail!(
                "unknown command '{name}' — available: {}",
                known_names(&commands)
            );
        };
        // `model_invocable: false` — the user's `:name` is the only way in (it is
        // also absent from the prompt listing, so this is reachable only by a model
        // that guessed the name). Point at the user rather than refusing flatly:
        // asking them to run `:{name}` is the useful next move, and is exactly why
        // the command is marked.
        if !command.model_invocable {
            anyhow::bail!(
                "command '{name}' is user-invocable only (`model_invocable: false`), so you cannot \
                 load it. If the task needs it, ask the user to run `:{name}` themselves."
            );
        }
        // Name the source: a built-in is hrdr's own text, anything else came off
        // disk from this project or the user's config. Same trust as `AGENTS.md`,
        // and stated so the model knows whose procedure it is following.
        let body = format!(
            "Command `{}` (source: {}) — instructions from the user or this project; follow them \
             for this task.\n\n{}",
            command.name,
            command.source,
            expand_body(command, arguments)
        );
        Ok(hrdr_tools::truncate_saved(
            &body,
            COMMAND_OUTPUT_MAX_BYTES,
            usize::MAX,
            hrdr_tools::TruncateSide::Head,
            "command",
        ))
    }
}

/// Command names for an unknown-name error: all of them while that is a readable
/// list, otherwise the first few plus a count — a wall of names is how a
/// half-remembered name gets matched onto the wrong command.
fn known_names(commands: &[Command]) -> String {
    const SHOWN: usize = 40;
    let names: Vec<&str> = commands.iter().map(|s| s.name.as_str()).collect();
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

    fn command(name: &str, desc: &str, body: &str) -> Command {
        Command {
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
        let s = parse_command_file(
            "---\nname: ship\ndescription: release checklist\n---\nDo the release.",
            "stem",
            "src",
        )
        .unwrap();
        assert_eq!(s.name, "ship");
        assert_eq!(s.description, "release checklist");
        assert_eq!(s.body, "Do the release.");

        // No frontmatter: the stem names it, the whole text is the body.
        let s = parse_command_file("Just a prompt.", "quick", "src").unwrap();
        assert_eq!(s.name, "quick");
        assert_eq!(s.body, "Just a prompt.");

        // Empty body → not a command.
        assert!(parse_command_file("---\nname: x\n---\n  \n", "x", "src").is_none());

        // `args:` declares completion candidates (bracketed or bare list).
        let s = parse_command_file(
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
        let s = parse_command_file(
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
        let folded = parse_command_file(
            "---\ndescription: >\n  line one\n  line two\n---\nBody.",
            "stem",
            "src",
        )
        .unwrap();
        assert_eq!(folded.description, "line one line two");

        let literal = parse_command_file(
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
        let s = parse_command_file(
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
        let s = parse_command_file(
            "---\nargs: staging, production\n---\nDeploy $ARGUMENTS",
            "deploy",
            "src",
        )
        .unwrap();
        assert_eq!(s.args, vec!["staging", "production"]);
    }

    /// Frontmatter that isn't valid YAML (a tab-indented line — tabs are
    /// illegal for YAML indentation) degrades gracefully: the command still
    /// loads with a stem-derived name and the body intact, and — crucially —
    /// none of the raw frontmatter text leaks into the body sent to the
    /// model.
    #[test]
    fn invalid_yaml_frontmatter_degrades_without_leaking_into_body() {
        let s = parse_command_file(
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
        let s = parse_command_file(
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
            parse_command_file("---\njust a plain string\n---\nBody text.", "stem", "src").unwrap();
        assert_eq!(s.name, "stem");
        assert_eq!(s.description, "");
        assert!(s.args.is_empty());
        assert_eq!(s.body, "Body text.");
    }

    /// Security regression: a CRLF-authored command file (`---\r\n`) must still
    /// have its frontmatter parsed rather than falling through to "no fence",
    /// which would make the raw YAML (`name:`, `description:`, …) part of the
    /// prompt body sent to the model — covered by [`crate::split_fence`]'s
    /// own CRLF handling, shared with `agents_dir.rs`'s `split_frontmatter`.
    #[test]
    fn crlf_frontmatter_is_still_parsed() {
        let s = parse_command_file(
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
        let commands = vec![
            command("review", "", "Review the diff.\nFocus: $ARGUMENTS"),
            command("ship", "", "Run the release checklist."),
        ];
        // $ARGUMENTS placeholder is substituted (matched case-insensitively).
        assert_eq!(
            expand_command(":Review error handling", &commands).unwrap(),
            "Review the diff.\nFocus: error handling"
        );
        // No placeholder: args append on their own line…
        assert_eq!(
            expand_command(":ship v2 only", &commands).unwrap(),
            "Run the release checklist.\n\nv2 only"
        );
        // …and no args leaves the body untouched.
        assert_eq!(
            expand_command(":ship", &commands).unwrap(),
            "Run the release checklist."
        );
        // Unknown name / not an invocation → None (sent to the model as-is).
        assert!(expand_command(":nope", &commands).is_none());
        assert!(expand_command("hello :ship", &commands).is_none());
        assert!(expand_command(": ship", &commands).is_none());
    }

    /// A command that declares `args:` consumes only its first token as the
    /// argument; any text after it is appended to the body as extra context.
    #[test]
    fn declared_args_command_splits_arg_from_trailing_context() {
        let mut audit = command("audit", "", "Audit at depth $ARGUMENTS.");
        audit.args = vec!["low".into(), "high".into()];
        let commands = vec![audit];

        // First token fills $ARGUMENTS; the rest is appended on its own line.
        assert_eq!(
            expand_command(":audit high focus on the parser", &commands).unwrap(),
            "Audit at depth high.\n\nfocus on the parser"
        );
        // Just the arg, no trailing context: nothing is appended.
        assert_eq!(
            expand_command(":audit low", &commands).unwrap(),
            "Audit at depth low."
        );
        // No arg at all: $ARGUMENTS renders empty, as before.
        assert_eq!(
            expand_command(":audit", &commands).unwrap(),
            "Audit at depth ."
        );
    }

    /// A command with `args:` but no `$ARGUMENTS` placeholder still appends the
    /// first token (existing no-placeholder behavior) followed by any extra.
    #[test]
    fn declared_args_command_without_placeholder_appends_both() {
        let mut s = command("audit", "", "Run the audit.");
        s.args = vec!["low".into(), "high".into()];
        let commands = vec![s];
        assert_eq!(
            expand_command(":audit high and check the auth flow", &commands).unwrap(),
            "Run the audit.\n\nhigh\n\nand check the auth flow"
        );
    }

    /// A command WITHOUT `args:` is unchanged: the whole remainder is one argument
    /// and is not split on the first space (a pasted error, a commit scope).
    #[test]
    fn free_form_command_keeps_whole_remainder_as_one_argument() {
        let commands = vec![command("fix", "", "Fix this: $ARGUMENTS")];
        assert_eq!(
            expand_command(":fix TypeError at line 5 in foo.rs", &commands).unwrap(),
            "Fix this: TypeError at line 5 in foo.rs"
        );
    }

    /// The `command` tool and a `:` invocation share [`expand_body`], so the same
    /// command and the same argument text expand to the same bytes whichever way
    /// it was invoked.
    #[test]
    fn expand_body_matches_a_colon_invocation() {
        let mut audit = command("audit", "", "Audit at depth $ARGUMENTS.");
        audit.args = vec!["low".into(), "high".into()];
        let commands = vec![audit.clone()];
        assert_eq!(
            expand_body(&audit, "high focus on the parser"),
            expand_command(":audit high focus on the parser", &commands).unwrap()
        );
        assert_eq!(
            expand_body(&audit, ""),
            expand_command(":audit", &commands).unwrap()
        );
    }

    #[test]
    fn discovery_dedupes_by_name_project_first() {
        let dir = tempfile::tempdir().unwrap();
        let hrdr = dir.path().join(".hrdr/commands");
        let claude = dir.path().join(".claude/commands");
        std::fs::create_dir_all(&hrdr).unwrap();
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(hrdr.join("ship.md"), "hrdr wins").unwrap();
        std::fs::write(claude.join("ship.md"), "claude loses").unwrap();
        std::fs::write(claude.join("review.md"), "review the diff").unwrap();
        std::fs::write(claude.join("notes.txt"), "not a command").unwrap();

        let commands = discover_commands(dir.path(), crate::prompt::ProjectInstructions::Load);
        let ship = commands.iter().find(|s| s.name == "ship").unwrap();
        assert_eq!(ship.body, "hrdr wins", "project .hrdr dir outranks .claude");
        assert!(commands.iter().any(|s| s.name == "review"));
        assert!(!commands.iter().any(|s| s.name == "notes"));
    }

    /// A command dir is walked recursively and a nested file is named by its
    /// path (`git/commit.md` → `git/commit`), which is what makes `:git/commit`
    /// an invocation rather than a miss.
    #[test]
    fn nested_files_are_namespaced_by_their_path() {
        let dir = tempfile::tempdir().unwrap();
        let hrdr = dir.path().join(".hrdr/commands");
        std::fs::create_dir_all(hrdr.join("git")).unwrap();
        std::fs::write(hrdr.join("ship.md"), "top level").unwrap();
        std::fs::write(hrdr.join("git/commit.md"), "commit the tree").unwrap();

        let commands = discover_commands(dir.path(), crate::prompt::ProjectInstructions::Load);
        assert!(commands.iter().any(|c| c.name == "ship"));
        let nested = commands
            .iter()
            .find(|c| c.name == "git/commit")
            .expect("nested file is namespaced by its directory");
        assert_eq!(nested.body, "commit the tree");

        // All three separators reach it, and each expands to the same body.
        for input in [":git/commit", ":git:commit", ":git.commit", ":GIT/Commit"] {
            assert_eq!(
                expand_command(input, &commands).as_deref(),
                Some("commit the tree"),
                "{input} must resolve"
            );
        }
    }

    /// A frontmatter `name:` replaces the path-derived name outright — it is
    /// not prefixed with the directory the file was found in.
    #[test]
    fn frontmatter_name_overrides_the_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let hrdr = dir.path().join(".hrdr/commands");
        std::fs::create_dir_all(hrdr.join("git")).unwrap();
        std::fs::write(hrdr.join("git/commit.md"), "---\nname: ship\n---\nShip it.").unwrap();

        let commands = discover_commands(dir.path(), crate::prompt::ProjectInstructions::Load);
        assert!(commands.iter().any(|c| c.name == "ship"));
        assert!(!commands.iter().any(|c| c.name == "git/commit"));
    }

    /// Two spellings of one namespaced name are the same command, not two: the
    /// dedup compares [`command_match_key`], so `git.commit` cannot sneak past
    /// the `git/commit` that already won its slot.
    #[test]
    fn separator_spellings_collapse_to_one_command() {
        let dir = tempfile::tempdir().unwrap();
        let hrdr = dir.path().join(".hrdr/commands");
        std::fs::create_dir_all(hrdr.join("git")).unwrap();
        std::fs::write(hrdr.join("git/commit.md"), "the nested one").unwrap();
        std::fs::write(hrdr.join("git.commit.md"), "the dotted one").unwrap();

        let commands = discover_commands(dir.path(), crate::prompt::ProjectInstructions::Load);
        let matches: Vec<&Command> = commands
            .iter()
            .filter(|c| command_match_key(&c.name) == "git/commit")
            .collect();
        assert_eq!(matches.len(), 1, "one slot per key: {matches:?}");
    }

    /// **A symlinked command dir IS walked; a symlink INSIDE one is not.**
    ///
    /// `discover_commands` builds its walk with `ignore`'s default
    /// `follow_links(false)`, but walkdir follows a symlinked *root* regardless
    /// (`follow_root_links` defaults on) and `WalkBuilder` leaves that default
    /// alone. So `~/.config/hrdr/commands -> ~/dotfiles/commands` — the dotfiles
    /// layout — does contribute its files, while a `shared -> …` link dropped
    /// inside a command dir contributes nothing. Both halves are asserted
    /// because the two look identical from outside and behave oppositely.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_dir_is_walked_but_a_symlink_inside_one_is_not() {
        let dotfiles = tempfile::tempdir().unwrap();
        std::fs::write(dotfiles.path().join("ship.md"), "ship it").unwrap();

        let linked = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(linked.path().join(".hrdr")).unwrap();
        std::os::unix::fs::symlink(dotfiles.path(), linked.path().join(".hrdr/commands")).unwrap();
        let commands = discover_commands(linked.path(), crate::prompt::ProjectInstructions::Load);
        assert!(
            commands.iter().any(|c| c.name == "ship"),
            "a symlinked command dir IS walked"
        );

        let nested = tempfile::tempdir().unwrap();
        let dir = nested.path().join(".hrdr/commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(dotfiles.path(), dir.join("shared")).unwrap();
        let commands = discover_commands(nested.path(), crate::prompt::ProjectInstructions::Load);
        assert!(
            !commands.iter().any(|c| c.name == "shared/ship"),
            "a symlink below the dir is NOT followed"
        );
        assert!(
            commands.iter().all(|c| c.source == "built-in"),
            "…so that dir contributes nothing at all"
        );
    }

    /// A symlink cycle inside a command dir must not hang discovery — a
    /// liveness guard, so the failure it catches is the run never returning
    /// rather than an assertion. Today the walk follows no link below the dir
    /// itself; this is what would fail if a later walk did, without cycle
    /// detection.
    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_does_not_hang_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let hrdr = dir.path().join(".hrdr/commands");
        std::fs::create_dir_all(&hrdr).unwrap();
        std::fs::write(hrdr.join("ship.md"), "ship it").unwrap();
        std::os::unix::fs::symlink(&hrdr, hrdr.join("loop")).unwrap();

        let commands = discover_commands(dir.path(), crate::prompt::ProjectInstructions::Load);
        assert!(commands.iter().any(|c| c.name == "ship"));
    }

    /// The built-in templates each parse into a usable command: a name,
    /// a non-empty description and body, and — for `release`/`review`/`audit`, whose
    /// templates declare `args:` — the completion candidates the popup should
    /// offer after `:name `. The rest declare none, so their lists are empty.
    ///
    /// The registry is generated by `build.rs` from the templates directory, so
    /// the expected count is read from that same directory: adding a built-in
    /// command is adding a file, and this test follows along without an edit.
    #[test]
    fn builtins_parse_with_names_descriptions_bodies_and_args() {
        let commands = builtin_commands();
        // Every `*.md` template on disk is registered, and nothing else is.
        let template_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/templates/commands");
        let expected = std::fs::read_dir(&template_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
            .count();
        assert_eq!(
            commands.len(),
            expected,
            "one registered command per template file"
        );

        for s in &commands {
            assert!(!s.description.is_empty(), "{} description", s.name);
            assert!(!s.body.is_empty(), "{} body", s.name);
            assert_eq!(s.source, "built-in");
        }
        // The generic runner and its discovery helper anchor the dependency
        // workflow: `:deps` routes through `:cli`, which learns any tool from
        // the machine's own help rather than a curated run book.
        for name in ["deps", "cli"] {
            assert!(
                commands.iter().any(|s| s.name == name),
                "missing built-in {name}"
            );
        }

        assert!(
            commands
                .iter()
                .find(|s| s.name == "commit")
                .unwrap()
                .args
                .is_empty(),
            "commit declares no args"
        );
        assert_eq!(
            commands.iter().find(|s| s.name == "release").unwrap().args,
            vec!["patch", "minor", "major"]
        );
        assert_eq!(
            commands.iter().find(|s| s.name == "review").unwrap().args,
            vec!["low", "high"]
        );
        assert_eq!(
            commands.iter().find(|s| s.name == "audit").unwrap().args,
            vec!["low", "high"]
        );
        for name in ["fix", "test", "todo", "plan", "tidy", "perf", "sweep"] {
            assert!(
                commands
                    .iter()
                    .find(|s| s.name == name)
                    .unwrap()
                    .args
                    .is_empty(),
                "{name} declares no args"
            );
        }
    }

    /// `discover_commands` on a cwd with no command directories at all still
    /// returns the built-ins — the whole point of shipping them is that the
    /// built-in set works with zero setup. The expected set is read from the
    /// templates directory, the same source `build.rs` generates the registry
    /// from, so a new command file updates this expectation with it.
    #[test]
    fn discover_commands_on_empty_cwd_returns_only_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let commands = discover_commands(dir.path(), crate::prompt::ProjectInstructions::Load);
        let mut names: Vec<String> = commands.iter().map(|s| s.name.clone()).collect();
        names.sort();
        let mut expected: Vec<String> =
            std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/templates/commands"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
                .map(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .trim_end_matches(".md")
                        .to_string()
                })
                .collect();
        expected.sort();
        assert_eq!(names, expected);
        assert!(commands.iter().all(|s| s.source == "built-in"));
    }

    /// A project's own `.hrdr/commands/commit.md` shadows the built-in `commit`
    /// — built-ins are appended last in `discover_commands`, so they only fill
    /// gaps the dedup (first source wins, case-insensitive) leaves open.
    #[test]
    fn project_command_overrides_the_builtin_of_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let hrdr = dir.path().join(".hrdr/commands");
        std::fs::create_dir_all(&hrdr).unwrap();
        std::fs::write(hrdr.join("commit.md"), "project commit wins").unwrap();

        let commands = discover_commands(dir.path(), crate::prompt::ProjectInstructions::Load);
        let commit = commands.iter().find(|s| s.name == "commit").unwrap();
        assert_eq!(commit.body, "project commit wins");
        assert_ne!(commit.source, "built-in");
        // The other built-ins are still present, unshadowed.
        assert!(
            commands
                .iter()
                .any(|s| s.name == "release" && s.source == "built-in")
        );
        assert!(
            commands
                .iter()
                .any(|s| s.name == "review" && s.source == "built-in")
        );
    }

    /// The tool that makes commands model-invocable: it resolves a name against the
    /// shared set and returns the expanded body, framed with its source so the
    /// model knows whose procedure it is following.
    #[tokio::test]
    async fn command_tool_loads_a_body_and_fills_arguments() {
        use hrdr_tools::Tool;
        let mut audit = command("audit", "audit the code", "Audit at depth $ARGUMENTS.");
        audit.args = vec!["low".into(), "high".into()];
        let tool = CommandTool {
            commands: Arc::new(Mutex::new(vec![audit])),
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
        assert!(out.contains("Command `audit` (source: test)"), "{out}");

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
    async fn command_tool_refuses_a_user_only_command() {
        use hrdr_tools::Tool;
        let mut release = command("release", "cut a release", "Bump, tag, push.");
        release.model_invocable = false;
        let tool = CommandTool {
            commands: Arc::new(Mutex::new(vec![release])),
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
        let builtins = builtin_commands();
        let flag = |name: &str| {
            builtins
                .iter()
                .find(|s| s.name == name)
                .unwrap()
                .model_invocable
        };
        for name in [
            "audit", "commit", "fix", "perf", "plan", "release", "review", "test", "tickets",
            "tidy", "todo",
        ] {
            assert!(flag(name), "{name} stays model-invocable");
        }
    }

    /// The frontmatter flag fails **open**: only a literal `false` opts out, so a
    /// typo or a stray string leaves the command loadable rather than silently
    /// hiding it.
    #[test]
    fn model_invocable_only_a_literal_false_opts_out() {
        let parse = |fm: &str| {
            parse_command_file(&format!("---\n{fm}\n---\nBody."), "s", "src")
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
            parse_command_file("Just a body.", "s", "src")
                .unwrap()
                .model_invocable
        );
    }

    /// An unknown name is an error that names what *is* available — the model
    /// half-remembering `:commits` must land on the list, not on nothing.
    #[tokio::test]
    async fn command_tool_rejects_an_unknown_name_and_lists_the_known_ones() {
        use hrdr_tools::Tool;
        let tool = CommandTool {
            commands: Arc::new(Mutex::new(vec![command("commit", "", "body")])),
        };
        let ctx = hrdr_tools::ToolContext::new(std::env::temp_dir());
        let err = tool
            .execute(serde_json::json!({"name": "commits"}), &ctx)
            .await
            .unwrap_err();
        let shown = format!("{err:#}");
        assert!(shown.contains("unknown command 'commits'"), "{shown}");
        assert!(shown.contains("available: commit"), "{shown}");

        // A missing/blank name is an error too, not a silent first-command pick.
        assert!(
            tool.execute(serde_json::json!({"name": "  "}), &ctx)
                .await
                .is_err()
        );
        assert!(tool.execute(serde_json::json!({}), &ctx).await.is_err());
    }

    /// The unknown-name list is bounded: a directory of many commands must not dump
    /// every name into one error, which is how a half-remembered name gets matched
    /// onto the wrong command.
    #[test]
    fn unknown_name_list_is_bounded() {
        let many: Vec<Command> = (0..100)
            .map(|i| command(&format!("s{i:03}"), "", "b"))
            .collect();
        let listed = known_names(&many);
        assert!(listed.contains("s000"));
        assert!(listed.contains("(60 more)"), "{listed}");
        assert!(!listed.contains("s099"), "the tail is counted, not named");
        // A readable list is shown whole.
        assert_eq!(known_names(&many[..3]), "s000, s001, s002");
    }

    /// A command dir holding far more than `MAX_COMMANDS` files yields a bounded
    /// set: discovery stops at the aggregate file-count cap rather than reading
    /// every file. The project `.hrdr/commands` dir is scanned first and fills the
    /// budget, so the cap bites there (no reliance on the machine's user dirs);
    /// the built-ins are still appended afterwards. Half the files sit a
    /// directory down, so the cap is shown to be spent per file *visited* —
    /// nesting cannot buy a walk more of the budget.
    #[test]
    fn discover_commands_caps_the_file_count() {
        let dir = tempfile::tempdir().unwrap();
        let commands_dir = dir.path().join(".hrdr/commands");
        std::fs::create_dir_all(commands_dir.join("nested")).unwrap();
        for i in 0..(MAX_COMMANDS + 50) {
            let name = format!("command{i:04}.md");
            let path = if i % 2 == 0 {
                commands_dir.join(name)
            } else {
                commands_dir.join("nested").join(name)
            };
            std::fs::write(path, format!("Body for command {i}.")).unwrap();
        }
        let commands = discover_commands(dir.path(), crate::prompt::ProjectInstructions::Load);
        let discovered = commands.iter().filter(|s| s.source != "built-in").count();
        assert_eq!(
            discovered, MAX_COMMANDS,
            "command ingestion must stop at the aggregate file-count cap"
        );
        // The built-ins survive the cap — they're appended unconditionally.
        assert!(
            commands
                .iter()
                .any(|s| s.name == "commit" && s.source == "built-in")
        );
    }

    /// A dir holding more bytes than `MAX_COMMANDS_TOTAL_BYTES` yields a bounded
    /// set even while the file COUNT stays far under `MAX_COMMANDS`: the two
    /// ceilings are independent, and this is the one a few big files hit.
    #[test]
    fn discover_commands_caps_the_total_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let commands_dir = dir.path().join(".hrdr/commands");
        std::fs::create_dir_all(&commands_dir).unwrap();
        // Each file at the per-file ceiling, so the aggregate budget buys exactly
        // this many before it is spent.
        let per_file = MAX_COMMAND_FILE_BYTES as usize;
        let affordable = MAX_COMMANDS_TOTAL_BYTES / per_file;
        for i in 0..(affordable + 6) {
            std::fs::write(
                commands_dir.join(format!("command{i:04}.md")),
                "b".repeat(per_file),
            )
            .unwrap();
        }
        let commands = discover_commands(dir.path(), crate::prompt::ProjectInstructions::Load);
        let discovered = commands.iter().filter(|c| c.source != "built-in").count();
        assert_eq!(discovered, affordable);
        assert!(
            affordable < MAX_COMMANDS,
            "the byte cap is what bit here, not the file-count cap"
        );
    }

    /// A command file over `MAX_COMMAND_FILE_BYTES` is skipped before it is
    /// read. The cap is a ceiling, so a file of exactly that size still loads.
    #[test]
    fn discover_commands_refuses_a_file_over_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let commands_dir = dir.path().join(".hrdr/commands");
        std::fs::create_dir_all(&commands_dir).unwrap();
        let cap = MAX_COMMAND_FILE_BYTES as usize;
        std::fs::write(commands_dir.join("huge.md"), "b".repeat(cap + 1)).unwrap();
        std::fs::write(commands_dir.join("atcap.md"), "b".repeat(cap)).unwrap();

        let commands = discover_commands(dir.path(), crate::prompt::ProjectInstructions::Load);
        assert!(!commands.iter().any(|c| c.name == "huge"));
        assert!(
            commands.iter().any(|c| c.name == "atcap"),
            "a file at exactly the cap is within it"
        );
    }
}

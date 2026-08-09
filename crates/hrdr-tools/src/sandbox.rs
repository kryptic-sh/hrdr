//! Filesystem confinement: how much of the disk an agent may touch.
//!
//! Two layers share one vocabulary. [`SandboxPolicy`] is the resolved boundary
//! — a mode plus concrete, canonicalized root sets — consulted by the
//! in-process file tools (software guard) and, on Linux, handed to the OS for
//! shell children (Landlock on Linux, Seatbelt on macOS). This module owns the
//! vocabulary, the path
//! mechanics, and — via [`sandboxed_shell_command`] — the OS wrapper the
//! `shell` and `watch` tools spawn through.
//!
//! The two layers are not redundant: the software guard sees only the paths a
//! tool is *handed*, and a shell command is opaque to it. Everything a
//! subprocess writes is the OS layer's job.
//!
//! The reason the boundary is *enforced* rather than *asked for*: hrdr runs
//! arbitrary models, and guidance only reaches steerable ones. A delegated
//! sub-agent that `cd`s out of its worktree and commits to the parent repo's
//! `main` is the concrete failure this exists to make impossible.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::{canonicalize_nearest, tool_output_dir};

/// How much of the filesystem an agent may touch. Enforced by the OS for
/// shell children (Landlock/Seatbelt) and by a software path-guard for
/// the in-process file tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    /// No confinement — full read/write everywhere. The pre-sandbox behavior.
    ///
    /// Spelled `none`, `yolo` or `off` in config/env/flags; `none` is canonical
    /// and the one this renders back as.
    None,
    /// Read broadly (builds need /usr, toolchains, ~/.cargo, …); write ONLY
    /// within the writable roots (cwd + temp/scratch + tool-output dir + git
    /// metadata roots for a linked worktree + configured extras).
    Write,
    /// Read broadly, write NOWHERE. What a read-only agent gets.
    ///
    /// "Read-only" is a restriction on WRITING, not on reading — the same
    /// meaning Codex gives its `read-only` mode, and for the same reason: a
    /// review agent has to run the tools the user installed, and those live all
    /// over the filesystem (`~/.cargo/bin`, a nvm/fnm node, a Homebrew or Nix
    /// prefix, a mason symlink farm). This mode used to confine reads too, which
    /// left an agent's shell able to see only `/usr` and `/etc` — "command not
    /// found" for tools that are plainly installed. [`Jail`](Self::Jail) is that
    /// behavior, kept and made opt-in.
    Read,
    /// Read only within the readable roots (its working directory and its own
    /// output dir), write nowhere, and hold **only the read-only tools** — no
    /// `shell`, no `verify`, no LSP, no `web_fetch`/`web_search`, no MCP, no
    /// `task`, no `memory`. *You read, you do not run.*
    ///
    /// The strongest confinement hrdr has, and **opt-in** (`sandbox = "jail"`).
    /// It exists for one job: inspecting third-party code you are unwilling to
    /// expose to. The threat is not that the agent is untrustworthy — it is that
    /// **the code it reads may act through it**, so a project-wide readable root
    /// would let audited content saying "append `../../.env` to your report" be
    /// complied with.
    ///
    /// Confinement is entirely **in-process**, which is what makes it work on
    /// every platform with no OS backend at all: with nothing that spawns a
    /// subprocess, [`check_read`](SandboxPolicy::check_read) on the canonical path
    /// is the whole boundary. That is also the honest answer to why nothing is
    /// writable — with no execution there is nothing that needs a writable
    /// `/tmp`.
    ///
    /// The accepted loss is `git log` on the audited repo, real provenance value.
    /// That argues for a narrow read-only git capability later, not a general
    /// shell now.
    Jail,
}

impl SandboxMode {
    /// The canonical spelling, matching the config/env/flag vocabulary.
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxMode::None => "none",
            SandboxMode::Write => "write",
            SandboxMode::Read => "read",
            SandboxMode::Jail => "jail",
        }
    }
}

impl std::str::FromStr for SandboxMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "write" => Ok(SandboxMode::Write),
            "read" => Ok(SandboxMode::Read),
            "jail" => Ok(SandboxMode::Jail),
            // `yolo` is a SPELLING of `none`, not a fourth behavior: turning the
            // sandbox off is already exactly one thing, and two modes that did
            // the same thing under different names would be a bug waiting to be
            // written. It exists because that is the word people reach for, and
            // a mode you cannot name is one you disable some other, worse way.
            // `none` stays canonical — it is what `as_str`/`Display` render.
            "none" | "yolo" | "off" => Ok(SandboxMode::None),
            other => Err(format!(
                "unknown sandbox mode {other:?} — expected write, read, jail, or none \
                 (aka yolo/off)"
            )),
        }
    }
}

impl std::fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A resolved confinement policy: the mode plus the concrete, canonicalized
/// root sets. Built once per agent in `Agent::new`; `ToolContext` holds it
/// behind an Arc so tool calls share it cheaply.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    /// Canonicalized (via [`canonicalize_nearest`]) writable roots. Empty when
    /// mode is `None` (meaning "everything") or `Read` (meaning "nothing").
    pub writable_roots: Vec<PathBuf>,
    /// Canonicalized readable roots; only consulted in `Read` mode.
    pub readable_roots: Vec<PathBuf>,
    /// Whether every tool result is wrapped in an untrusted-content envelope
    /// ([`crate::wrap_untrusted`]) before the model sees it.
    ///
    /// Always true in [`SandboxMode::Jail`], where the premise is that the content
    /// under audit may try to act through the agent, and where the envelope's
    /// `source` label is exactly what you want attached to every byte. Settable
    /// from config in the other modes too — it is one bool, and wanting it does not
    /// mean wanting a whole different mode.
    ///
    /// **Read in exactly one place**, [`crate::ToolRegistry::execute`], because
    /// that is the only place every tool passes through. Two readers would be two
    /// chances to disagree about whether a payload was already wrapped.
    pub wrap_tool_results: bool,
    /// Which of [`writable_roots`](Self::writable_roots) are package-manager
    /// caches ([`package_cache_roots`]).
    ///
    /// A **rendering label, never a boundary**: every path here is also in
    /// `writable_roots`, and enforcement reads only that. The duplication is
    /// deliberate — a separate set the OS layer had to remember to consult is one
    /// forgotten call away from a hole, whereas a label nobody consults for
    /// permission cannot open one.
    ///
    /// It exists because these roots are machinery, not choices. Two dozen cache
    /// paths in the system prompt's "you may write only under" list is noise the
    /// model has to read on every turn, and the model never decides to write
    /// there — `cargo` and `npm` do. So prompts and refusals name
    /// [`project_writable_roots`](Self::project_writable_roots) and summarize the
    /// rest in one clause.
    pub cache_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// The no-op policy: mode None, no roots. What `ToolContext::new` installs
    /// — the bare constructor stays unconfined on purpose; only `Agent::new`
    /// installs a real policy.
    pub fn unconfined() -> Self {
        Self {
            mode: SandboxMode::None,
            writable_roots: Vec::new(),
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        }
    }

    /// Build the policy for an agent working in `cwd`.
    ///
    /// Writable (mode `Write` only): `cwd`, [`std::env::temp_dir`],
    /// [`session_scratch_dir`], [`tool_output_dir`], the git metadata roots a
    /// linked worktree needs to commit (see [`git_metadata_roots`]), the
    /// package-manager caches ([`package_cache_roots`]), then the caller's
    /// configured `extras`. Every root is run through [`canonicalize_nearest`]
    /// and deduped (a root already under an earlier root is dropped).
    ///
    /// Readable roots (`cwd`, scratch, tool-output) are only ever CONSULTED in
    /// [`SandboxMode::Jail`] — the one mode that confines reads. `Read` and
    /// `Write` both read broadly, so they carry the list but never check it.
    ///
    /// Non-existent `extras` are skipped silently — a user config typo is not
    /// worth failing a session over, and everything in the default set is
    /// created by its own accessor before canonicalization.
    pub fn for_agent(mode: SandboxMode, cwd: &Path, extras: &[PathBuf]) -> Self {
        if mode == SandboxMode::None {
            return Self::unconfined();
        }
        let scratch = session_scratch_dir().to_path_buf();
        let output = tool_output_dir();
        let readable_roots =
            canonical_roots(vec![cwd.to_path_buf(), scratch.clone(), output.clone()]);
        let (writable_roots, cache_roots) = if matches!(mode, SandboxMode::Read | SandboxMode::Jail)
        {
            (Vec::new(), Vec::new())
        } else {
            let caches = package_cache_roots();
            let mut roots = vec![cwd.to_path_buf(), std::env::temp_dir(), scratch, output];
            roots.extend(git_metadata_roots(cwd));
            roots.extend(enclosing_git_dir(cwd));
            roots.extend(caches.iter().cloned());
            roots.extend(extras.iter().filter(|p| p.exists()).cloned());
            let roots = canonical_roots(roots);
            // Labelled *after* canonicalization and intersected with what
            // survived it, so a cache root that a broader root swallowed (a
            // session whose cwd is `$HOME`) is not claimed as a separate root
            // the prompt then omits.
            let caches = canonical_roots(caches)
                .into_iter()
                .filter(|c| roots.contains(c))
                .collect();
            (roots, caches)
        };
        Self {
            mode,
            writable_roots,
            readable_roots,
            cache_roots,
            // Jail always wraps: the content it reads is the thing being distrusted.
            // Every other mode starts off and is turned on from config.
            wrap_tool_results: mode == SandboxMode::Jail,
        }
    }

    /// Grant read access to `roots` on top of what [`for_agent`](Self::for_agent)
    /// derived, canonicalized and deduped the same way (a root already covered by
    /// an existing one is dropped). Roots that are not directories are skipped —
    /// a location nobody has created is not worth a line in the prompt's
    /// "you may read only under" list.
    ///
    /// This exists for directories that must stay readable **in jail**, the one
    /// mode that confines reads: hrdr grants the Agent Skill roots here, because a
    /// jailed agent listing procedures it is then refused permission to open is
    /// worse off than one that was told nothing. It widens reads only — writes and
    /// execution are untouched.
    ///
    /// A no-op under [`SandboxMode::None`], which reads everything already and
    /// whose policy stays byte-identical to [`unconfined`](Self::unconfined).
    pub fn allow_read(&mut self, roots: Vec<PathBuf>) {
        if self.mode == SandboxMode::None {
            return;
        }
        let mut merged = std::mem::take(&mut self.readable_roots);
        merged.extend(roots.into_iter().filter(|p| p.is_dir()));
        self.readable_roots = canonical_roots(merged);
    }

    /// The writable roots worth naming to a human or a model: everything except
    /// the package-manager caches (see [`cache_roots`](Self::cache_roots)).
    ///
    /// Pair it with [`cache_roots_clause`](Self::cache_roots_clause), which says
    /// in one clause what this omits.
    pub fn project_writable_roots(&self) -> Vec<&Path> {
        self.writable_roots
            .iter()
            .filter(|root| !self.cache_roots.iter().any(|c| c == *root))
            .map(PathBuf::as_path)
            .collect()
    }

    /// One clause naming the caches [`project_writable_roots`] leaves out, or
    /// empty when none were granted. Written to be appended to a sentence.
    pub fn cache_roots_clause(&self) -> &'static str {
        if self.cache_roots.is_empty() {
            ""
        } else {
            ", plus this machine's package-manager caches (cargo, npm, pip, go, … \
             — so dependency fetches and builds work without asking)"
        }
    }

    /// Err unless `canon` (already run through [`canonicalize_nearest`]) is under
    /// a writable root. `shown` is the path as the model named it, so the refusal
    /// talks about what it asked for.
    ///
    /// The question is answered on the *canonical* path, which resolves symlinks
    /// and lexical `..` — that is what makes the check escape-proof rather than
    /// textual.
    ///
    /// Mode `None` answers nothing: the unconfined path stays byte-identical to
    /// the pre-sandbox behavior (see the
    /// [`ToolContext::new`](crate::ToolContext::new) rule).
    ///
    /// This guards the **model's file tools** and nothing else — `shell` does not
    /// come through here. That asymmetry is why there is no longer a `.git`
    /// carve-out on top of the root check: it refused the file tools a write that
    /// `shell` performed one `git config` away, so it stopped the honest path and
    /// nothing else, while refusing legitimate `.git/info/exclude` edits and hooks
    /// the user had asked for. Oversight of git belongs at the shell layer, where
    /// guardrails run.
    pub fn check_write(&self, canon: &Path, shown: &Path) -> anyhow::Result<()> {
        if self.mode == SandboxMode::None {
            return Ok(());
        }
        if !is_under_any(canon, &self.writable_roots) {
            anyhow::bail!(
                "sandbox: refusing to write {} — it is outside this agent's writable roots. \
                 You may write only under: {}{}. Keep work inside your working directory; \
                 use the scratch dir for throwaway files.",
                shown.display(),
                join_paths(&self.project_writable_roots()),
                self.cache_roots_clause()
            )
        }
        Ok(())
    }

    /// Err iff the mode is `Strict` and `canon` (already canonicalized) is
    /// outside every readable root. A no-op in every other mode — `Read` means
    /// "writes nowhere", not "reads nowhere", so like `Write` it reads broadly
    /// (builds and review tools read all over the filesystem).
    pub fn check_read(&self, canon: &Path, shown: &Path) -> anyhow::Result<()> {
        if self.mode != SandboxMode::Jail || is_under_any(canon, &self.readable_roots) {
            return Ok(());
        }
        anyhow::bail!(
            "sandbox: refusing to read {} — this agent is strictly confined and may read only \
             under: {}.",
            shown.display(),
            join_roots(&self.readable_roots)
        )
    }
}

/// Canonicalize every root and drop the ones already covered by an earlier
/// one, preserving order (the first root is the most meaningful — the cwd —
/// and the refusal message reads in that order).
fn canonical_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for root in roots {
        let canon = canonicalize_nearest(&root);
        if !out.iter().any(|kept| canon.starts_with(kept)) {
            out.push(canon);
        }
    }
    out
}

/// Whether `canon` sits under any of `roots`. Both sides have been through
/// [`canonicalize_nearest`], which resolves symlinks and lexical `..` in the
/// not-yet-existing suffix — that is what makes this check escape-proof.
fn is_under_any(canon: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| canon.starts_with(root))
}

/// The roots as the refusal messages list them.
fn join_roots(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// [`join_roots`] for a borrowed set — what
/// [`SandboxPolicy::project_writable_roots`] returns.
fn join_paths(paths: &[&Path]) -> String {
    paths
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The per-session scratch dir: `<temp_dir>/hrdr-scratch-<pid>-<8 hex rand>`,
/// created 0700 on first use, one per process (a session lives in one process;
/// sub-agents share the process, hence the shared scratch — by design).
///
/// The first call also sweeps stale `hrdr-scratch-<pid>-*` siblings whose pid
/// is no longer alive. That sweep *is* the teardown: there is deliberately no
/// exit handler (a killed process runs none anyway, and the OS tmp reaper is
/// the backstop). Sweep failures are ignored.
pub fn session_scratch_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let temp = std::env::temp_dir();
        let dir = temp.join(format!(
            "hrdr-scratch-{}-{}",
            std::process::id(),
            rand_hex8()
        ));
        sweep_stale_scratch(&temp, &dir);
        crate::ensure_private_dir(&dir);
        dir
    })
    .as_path()
}

/// Eight hex characters of randomness for a per-session directory name, so a
/// recycled pid cannot land on a directory somebody else created first.
pub(crate) fn rand_hex8() -> String {
    use rand::RngExt as _;
    let mut bytes = [0u8; 4];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Remove `hrdr-scratch-<pid>-*` directories in `temp` whose pid is gone,
/// skipping `keep` (this process's own, freshly named). Best-effort: every
/// error is ignored.
#[cfg(unix)]
fn sweep_stale_scratch(temp: &Path, keep: &Path) {
    for path in dead_pid_dirs(temp, "hrdr-scratch-", keep) {
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// Remove `<prefix><pid>-*` session directories in `parent` whose pid is gone
/// **and** which have not been touched for a day, skipping `keep`.
///
/// The age condition is the difference from [`sweep_stale_scratch`], and it is
/// there for **resume**: a resumed session is by definition one whose process is
/// dead, and its restored context still carries "full output saved to <path>"
/// pointers into that session's output dir. Reaping on a dead pid alone would
/// delete exactly the files a resume is about to want. Scratch has no equivalent
/// problem — nothing points into it across a restart.
///
/// Best-effort: every error is ignored, and an unreadable mtime counts as recent
/// (keeping a directory is cheaper than deleting one somebody needs).
#[cfg(unix)]
pub(crate) fn sweep_stale_session_dirs(parent: &Path, prefix: &str, keep: &Path) {
    const KEEP_FOR: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
    for path in dead_pid_dirs(parent, prefix, keep) {
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .and_then(|t| t.elapsed().map_err(std::io::Error::other))
            .is_ok_and(|age| age > KEEP_FOR);
        if stale {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Non-unix: no portable liveness probe, so leave stale dirs to the OS.
#[cfg(not(unix))]
pub(crate) fn sweep_stale_session_dirs(_parent: &Path, _prefix: &str, _keep: &Path) {}

/// Directories in `parent` named `<prefix><pid>-…` whose pid is no longer alive,
/// excluding `keep`.
///
/// Signal 0 probes for existence without delivering anything. Only `ESRCH` proves
/// the process is gone — `EPERM` means it is alive and owned by somebody else, and
/// that directory is not ours to reap.
#[cfg(unix)]
fn dead_pid_dirs(parent: &Path, prefix: &str, keep: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep {
            continue;
        }
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(prefix))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<i32>().ok())
        else {
            continue;
        };
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            out.push(path);
        }
    }
    out
}

/// Non-unix: no portable liveness probe, so leave stale dirs to the OS.
#[cfg(not(unix))]
fn sweep_stale_scratch(_temp: &Path, _keep: &Path) {}

/// Extra writable roots a linked git worktree needs to commit, and nothing
/// more. Empty when `<cwd>/.git` is a directory (a normal checkout: `.git` is
/// under cwd, already writable) or absent (not a repo).
///
/// A write sub-agent works in a linked worktree, where `<cwd>/.git` is a
/// *file* pointing at `<repo>/.git/worktrees/<name>/` and `git commit` writes
/// objects into the **parent** repo's `.git/objects` and moves a ref under
/// `.git/refs/heads/hrdr/…`. Without these roots every sub-agent commit dies
/// on EROFS; with the whole parent `.git` writable, `git -C <parent>
/// update-ref refs/heads/main …` re-opens the exact escape this feature
/// closes. So: the worktree's private gitdir, the append-only object store,
/// and the two `hrdr/` ref namespaces — never `common` itself, its `index`,
/// its `packed-refs`, its `config`, or any other branch's refs.
fn git_metadata_roots(cwd: &Path) -> Vec<PathBuf> {
    let dot_git = cwd.join(".git");
    if !dot_git.is_file() {
        return Vec::new();
    }
    // Malformed pointers fail open to "no extras": worse ergonomics for a
    // broken worktree, never a wider boundary.
    let Ok(text) = std::fs::read_to_string(&dot_git) else {
        return Vec::new();
    };
    let Some(target) = text
        .lines()
        .next()
        .and_then(|line| line.trim().strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Vec::new();
    };
    let gitdir = canonicalize_nearest(&crate::resolve_under(cwd, target));
    if !gitdir.is_dir() {
        return Vec::new();
    }
    let Ok(commondir) = std::fs::read_to_string(gitdir.join("commondir")) else {
        return Vec::new();
    };
    let common = canonicalize_nearest(&crate::resolve_under(&gitdir, commondir.trim()));
    if !common.is_dir() {
        return Vec::new();
    }
    let refs = common.join("refs").join("heads").join("hrdr");
    let logs = common.join("logs").join("refs").join("heads").join("hrdr");
    // They must exist to be granted (a Landlock rule is added by opening the
    // path) and to canonicalize to themselves; git creates them lazily on the
    // first task branch.
    let _ = std::fs::create_dir_all(&refs);
    let _ = std::fs::create_dir_all(&logs);
    vec![gitdir, common.join("objects"), refs, logs]
}

/// The `.git` directory of the repository `cwd` sits **inside**, when it is above
/// `cwd` rather than under it. Empty for a repo root (whose `.git` is already
/// covered by the cwd root) and for a directory in no repo at all.
///
/// Needed the moment a write agent's cwd can be *narrower* than the repository —
/// which `task`'s `cwd` argument introduced. Scope a write sub-agent to
/// `crates/foo` and the repo's `.git` is above its only writable root, so
/// `git add`/`commit` die on an EROFS deep inside git, about a path nobody
/// mentioned. Granting the metadata directory restores committing without
/// widening what the agent may *edit*: files outside its cwd stay read-only, which
/// is the entire point of scoping it.
///
/// Deliberately not the enclosing repo's whole worktree, and deliberately the
/// resolved `.git` — a linked worktree's `<cwd>/.git` is a *file*, handled by
/// [`git_metadata_roots`] instead.
fn enclosing_git_dir(cwd: &Path) -> Option<PathBuf> {
    let mut dir = canonicalize_nearest(cwd).parent().map(Path::to_path_buf);
    while let Some(d) = dir {
        let dot_git = d.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        // A `.git` *file* here is a linked worktree's pointer, and following it is
        // `git_metadata_roots`'s job — with a narrower grant than the whole gitdir.
        if dot_git.is_file() {
            return None;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

/// Home directory, cross-platform (`$HOME`, else `%USERPROFILE%`). `None` in an
/// environment with neither, where every home-relative default below drops out.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// `$var` as a directory, if it is set to a non-empty absolute path.
///
/// Absolute on purpose: a relative `CARGO_HOME` would resolve against whatever
/// cwd this process happens to have, which is not what the tool that reads it
/// will resolve it against.
fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// `$var` if set (see [`env_dir`]), else `<home>/<fallback>`.
///
/// **Resolving the override is not a nicety.** A hardcoded `~/.cargo/registry`
/// on a machine with `CARGO_HOME=/opt/cargo` grants nothing at all, and the
/// build then fails with exactly the confusing EROFS the grant exists to
/// prevent — silently, because the default *looks* present.
fn tool_home(var: &str, fallback: &str, home: Option<&Path>) -> Option<PathBuf> {
    env_dir(var).or_else(|| home.map(|h| h.join(fallback)))
}

/// Directories a package manager must be able to write for ordinary project
/// work — `cargo build`, `npm i`, `go build`, `mvn`, `pip install` — to succeed
/// under [`SandboxMode::Write`].
///
/// **The common case must work out of the box.** `sandbox_writable_roots` and
/// `--sandbox-writable-root` are the escape hatch for a bespoke layout, not the
/// mechanism by which mainstream tooling becomes usable. Two failures verified
/// under a sandbox with only cwd/temp/scratch/output writable:
///
/// ```text
/// error: failed to open `~/.cargo/registry/cache/…/anyhow-1.0.75.crate`
/// Caused by: Read-only file system (os error 30)
/// ```
/// ```text
/// npm error code EROFS
/// npm error path /home/…/.npm/_cacache/tmp/0b23206c
/// ```
///
/// Note *where* cargo fails: the download succeeded, and it died writing the
/// crate into the cache. A build whose dependencies happen to be cached passes,
/// so this works on a warm machine and fails on a cold one — or the first time a
/// dependency is added. The npm case is [`sandbox_denial`]'s founding incident
/// reproduced exactly.
///
/// One cross-cutting entry does most of the work — `$XDG_CACHE_HOME`, plus
/// `~/Library/Caches` on macOS — covering pip, uv, deno, `go-build`, yarn v1,
/// composer, node-gyp and cabal. The rest of the list is the non-XDG holdouts.
///
/// **Never grant a tool's home directory, only its cache.** Verified, not
/// assumed: `~/.local/share/uv/` holds `credentials/` beside its data,
/// `~/.nuget/` holds config beside `packages/`, `~/.cargo/credentials.toml` is
/// commonly a symlink to a secret store, and `~/.m2/settings.xml`,
/// `~/.gradle/gradle.properties`, `~/.gem/credentials`, `~/.bundle/config` and
/// `~/.composer/auth.json` are all credential-bearing. `~/.npm` is the one safe
/// whole grant (`_cacache`, `_logs`, `_npx`, `_prebuilds`; config lives in
/// `~/.npmrc`, outside it) — worth saying so, so nobody tidies the list into
/// symmetry.
///
/// **Deliberately excluded: anything that puts a binary on `PATH`** —
/// `$CARGO_HOME/bin`, `$GOPATH/bin`, `~/.local/bin`, `~/.bun/bin`, and
/// `$PNPM_HOME` itself (which is pnpm's global *bin* dir; only its `store`
/// subdirectory is granted). A binary on `PATH` is a persistence vector: the next
/// command the *user* runs could be the agent's. So `cargo install` and
/// `go install` fail by default, with [`sandbox_denial`] naming the flag —
/// installing a tool is machine setup, not project work. Language toolchain
/// managers (`~/.nvm`, `~/.pyenv`, `~/.rbenv`, `~/.asdf`, uv's managed pythons)
/// are out for the same reason.
///
/// `$RUSTUP_HOME/toolchains` is the deliberate exception: a `rust-toolchain.toml`
/// pinning an uninstalled version makes `cargo build` itself fail on a fresh
/// checkout, which is project work. The download is checksum-verified and those
/// binaries are not on `PATH` (the rustup shims in `$CARGO_HOME/bin` are, and
/// stay excluded). `settings.toml` stays out, so the default toolchain cannot be
/// switched.
///
/// **The risk being accepted, stated.** Permanently writable caches escape the
/// project boundary durably: poison `~/.cargo/registry` and builds in *other*
/// projects are affected, including ones the user later runs by hand. What blunts
/// it enough to accept is that both caches are content-addressed and
/// integrity-checked — cargo verifies `.crate` files against the index checksum
/// before extraction, npm's `_cacache` is keyed by integrity hash — so writing
/// garbage there fails verification rather than executing. And an agent with
/// `shell`, a network and a writable cwd can already add a dependency whose
/// `build.rs` does anything, so this is a second route to something already
/// reachable, not a new capability.
pub fn package_cache_roots() -> Vec<PathBuf> {
    let home = home_dir();
    let home = home.as_deref();
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut add = |p: Option<PathBuf>| {
        if let Some(p) = p {
            roots.push(p);
        }
    };
    let under = |dir: &str| home.map(|h| h.join(dir));
    // A cache that IS the tool's home directory (`~/.npm`, `~/.stack`) has no
    // parent marker to test, so its ecosystem is detected on `PATH` instead —
    // see [`ensure_cache_root`] for why the two rules differ.
    let installed = |cmd: &str, p: Option<PathBuf>| p.filter(|_| command_on_path(cmd));

    // Cross-cutting.
    add(tool_home("XDG_CACHE_HOME", ".cache", home));
    if cfg!(target_os = "macos") {
        add(under("Library/Caches"));
    }

    // Rust.
    let cargo = tool_home("CARGO_HOME", ".cargo", home);
    add(cargo.as_ref().map(|c| c.join("registry")));
    add(cargo.as_ref().map(|c| c.join("git")));
    let rustup = tool_home("RUSTUP_HOME", ".rustup", home);
    for sub in ["toolchains", "downloads", "tmp", "update-hashes"] {
        add(rustup.as_ref().map(|r| r.join(sub)));
    }

    // Node.
    add(installed("npm", under(".npm")));
    add(installed("node", under(".node-gyp")));
    add(env_dir("PNPM_HOME").map(|p| p.join("store")));
    add(under(".local/share/pnpm/store"));
    add(under("Library/pnpm/store"));
    add(installed("pnpm", under(".pnpm-store")));
    add(under(".yarn/berry/cache"));
    add(under(".bun/install/cache"));
    add(env_dir("DENO_DIR"));

    // Python.
    add(env_dir("UV_CACHE_DIR"));
    add(env_dir("PIP_CACHE_DIR"));
    add(under(".local/share/pypoetry/venvs"));
    add(under(".local/share/pipx"));

    // Go.
    add(env_dir("GOCACHE"));
    add(env_dir("GOMODCACHE").or_else(|| {
        env_dir("GOPATH")
            .or_else(|| under("go"))
            .map(|p| p.join("pkg").join("mod"))
    }));

    // JVM.
    add(under(".m2/repository"));
    let gradle = tool_home("GRADLE_USER_HOME", ".gradle", home);
    add(gradle.as_ref().map(|g| g.join("caches")));
    add(gradle.as_ref().map(|g| g.join("wrapper")));

    // .NET.
    add(env_dir("NUGET_PACKAGES").or_else(|| under(".nuget/packages")));

    // Ruby.
    add(under(".local/share/gem"));
    add(under(".gem/ruby"));
    add(under(".bundle/cache"));

    // PHP — the default is XDG; only an override needs naming.
    add(env_dir("COMPOSER_HOME").map(|c| c.join("cache")));

    // Dart.
    add(env_dir("PUB_CACHE").or_else(|| installed("dart", under(".pub-cache"))));

    // Elixir.
    add(under(".hex/packages"));
    add(installed("mix", under(".mix")));

    // Haskell.
    add(env_dir("STACK_ROOT").or_else(|| installed("stack", under(".stack"))));
    add(under(".cabal/packages"));

    roots.retain(|root| ensure_cache_root(root));
    roots
}

/// Whether `root` is a usable grant: it exists, or this created it.
///
/// **Creating it is what makes the grant real.** The OS layer can only confine a
/// path that exists — Landlock resolves a rule by opening it — so an absent root
/// is silently dropped, and the package manager cannot create it either, because
/// its parent is not writable. On a fresh machine `~/.npm` does not exist yet, so
/// the first `npm i` would fail with the same EROFS as if nothing had been
/// granted, *despite* the default being present.
///
/// Only when the immediate parent already exists, which is the line between
/// completing a layout and inventing one: `~/.cargo` exists exactly when cargo is
/// installed, so `~/.cargo/registry` is created on a machine that builds Rust and
/// skipped on one that never will. Without that, hrdr would scatter two dozen
/// empty package-manager directories through the home of anyone who runs it once.
/// The cost is bounded and named: a tool installed but never yet run — `mvn` with
/// no `~/.m2` — fails its first fetch with [`sandbox_denial`] pointing at
/// `--sandbox-writable-root`.
///
/// The rule needs a companion for the caches that ARE a tool's home directory
/// (`~/.npm`, `~/.stack`): their parent is `$HOME`, which always exists, so
/// parent-existence proves nothing and [`package_cache_roots`] gates those on the
/// tool being on `PATH` instead.
///
/// Failures are ignored, so a read-only `$HOME` degrades rather than aborting.
fn ensure_cache_root(root: &Path) -> bool {
    if root.is_dir() {
        return true;
    }
    if root.parent().is_some_and(Path::is_dir) {
        let _ = std::fs::create_dir_all(root);
    }
    root.is_dir()
}

/// Whether `cmd` is an executable file on `PATH` — a `which(1)` with no
/// subprocess, used to decide whether an ecosystem exists on this machine.
///
/// Deliberately does not consult a shell: aliases and functions are not what a
/// package manager's own child process will find, and spawning one per lookup at
/// session start would cost more than every stat here combined.
fn command_on_path(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(cmd);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::metadata(&candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            // No mode bits to test; PATHEXT-suffixed names are what exists.
            candidate.is_file()
                || ["exe", "cmd", "bat"]
                    .iter()
                    .any(|ext| candidate.with_extension(ext).is_file())
        }
    })
}

/// One agent's sandbox degradation notices awaiting delivery through **its own**
/// event stream, plus the ones it has already been told.
///
/// Per agent, not per process: a notice is a statement about *this* agent's
/// confinement, and several agents run in one process, each with its own
/// [`SandboxPolicy`]. A single shared queue let whichever turn loop drained
/// first swallow a sibling's notice — the wrong session hearing that its sandbox
/// degraded, the right one never hearing it, and a test
/// (`sandbox_notice_reaches_the_event_stream`) that failed whenever a parallel
/// test drained its seeded notice.
///
/// Lives beside the policy in [`crate::ToolContext`] rather than inside it: the
/// policy is an immutable *description* of a boundary, built once and shared
/// behind an `Arc` (other crates construct one as a plain literal to render it);
/// this is mutable per-session state.
///
/// The seen-set is the difference from `hrdr_llm::take_client_warning`'s plain
/// cell: a degradation is detected on *every* confined shell command, so a
/// bare slot would re-fill after each drain and the user would see the same
/// warning once per command. Each distinct message is delivered exactly once
/// per agent — the recurrence is silenced, the sibling is not.
///
/// Pending is a *queue* rather than a single slot because one command can
/// degrade twice — a read-mode agent on the Landlock fallback both loses its
/// primary backend and loses read confinement — and a single slot would
/// silently drop the first of the two while still marking it seen.
#[derive(Debug, Default)]
pub struct SandboxNotices {
    /// `(already told, awaiting delivery)`. A poisoned lock costs a notice
    /// rather than a panic, exactly as the process-global cell did: a
    /// degradation warning is not worth taking a session down for.
    inner: Mutex<(HashSet<String>, VecDeque<String>)>,
}

impl SandboxNotices {
    /// Record a degradation notice. Only a message *this agent* has not been
    /// told yet is queued; repeats are dropped.
    pub fn set(&self, msg: String) {
        if let Ok(mut cell) = self.inner.lock()
            && cell.0.insert(msg.clone())
        {
            cell.1.push_back(msg);
        }
    }

    /// Take the next pending notice for delivery through this agent's normal
    /// event channel (never stderr — a TUI owns the terminal).
    pub fn take(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|mut cell| cell.1.pop_front())
    }
}

/// Emitted when a confined agent's shell command runs without any OS-level
/// confinement — a Linux kernel without Landlock, a macOS whose
/// `/usr/bin/sandbox-exec` is gone, or Windows.
/// Never silently pretend to sandbox: the file tools stay guarded, the shell
/// does not.
const NO_OS_SANDBOX_NOTICE: &str = "sandbox: no OS-level sandbox is available on this system — \
     shell commands are NOT OS-confined; the file tools remain guarded. Use --sandbox none to \
     silence this.";

/// The OS mechanism available to confine *shell children* on this machine.
///
/// The file tools are guarded in-process regardless; this is only about the
/// subprocesses `shell` spawns, which the software guard cannot see inside of.
/// One mechanism per platform: Landlock on Linux, Seatbelt on macOS, nothing
/// anywhere else.
///
/// Read confinement is enforced in-process by [`SandboxPolicy::check_read`] in
/// `jail` mode; the OS backends confine writes only. No mode confines the
/// network — that is enforced at the tool level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsSandboxBackend {
    /// Landlock LSM rules the child applies to itself. Writes only.
    Landlock,
    /// macOS `sandbox-exec(1)` with a generated SBPL profile.
    Seatbelt,
    /// Windows Mandatory Integrity Control: the child lowers its own token to
    /// Low integrity, after which the kernel refuses every write to an object
    /// labelled Medium or higher — which is everything the user owns.
    ///
    /// Reads are unaffected (MIC's default policy is NO_WRITE_UP only), so this
    /// is exactly [`SandboxMode::Read`]'s shape and needs no filesystem change
    /// to deliver it. Granting writes back to the policy's writable roots is a
    /// second step (labelling those roots Low) and is NOT part of this variant
    /// yet — see [`low_integrity_args`].
    LowIntegrity,
    /// Nothing available: the shell runs unconfined and says so.
    None,
}

/// The backend this process uses, resolved once and cached.
pub fn detect_backend() -> OsSandboxBackend {
    static BACKEND: OnceLock<OsSandboxBackend> = OnceLock::new();
    *BACKEND.get_or_init(detect_backend_uncached)
}

/// Linux: Landlock if the LSM is enabled, else nothing.
///
/// Kernel 5.13+ (July 2021). Below that the shell runs unconfined with a notice,
/// the same posture Windows has — Debian 12 ships 6.1 and RHEL 9 ships 5.14, so
/// the band is narrow, but the notice must say so plainly.
#[cfg(target_os = "linux")]
fn detect_backend_uncached() -> OsSandboxBackend {
    if landlock_available() {
        OsSandboxBackend::Landlock
    } else {
        OsSandboxBackend::None
    }
}

/// Whether this kernel has the Landlock LSM enabled — the authoritative answer
/// being the list of active LSMs, not a probe.
#[cfg(target_os = "linux")]
fn landlock_available() -> bool {
    std::fs::read_to_string("/sys/kernel/security/lsm")
        .unwrap_or_default()
        .contains("landlock")
}

#[cfg(target_os = "macos")]
fn detect_backend_uncached() -> OsSandboxBackend {
    if Path::new(SEATBELT_PROGRAM).exists() {
        OsSandboxBackend::Seatbelt
    } else {
        OsSandboxBackend::None
    }
}

/// Windows: Mandatory Integrity Control, which every supported release has.
///
/// The only precondition is being able to name our own executable to re-exec —
/// the confinement is applied by a child of ours that lowers its own token (see
/// [`low_integrity_args`]), so an unresolvable `current_exe` means no backend
/// rather than a backend that silently confines nothing.
#[cfg(windows)]
fn detect_backend_uncached() -> OsSandboxBackend {
    match std::env::current_exe() {
        Ok(_) => OsSandboxBackend::LowIntegrity,
        Err(_) => OsSandboxBackend::None,
    }
}

/// Every other platform: nothing.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn detect_backend_uncached() -> OsSandboxBackend {
    OsSandboxBackend::None
}

/// The GPU/compute device nodes present on this host, for the sandbox to bind
/// through.
///
/// A GPU node is opened **read-write** to submit work at all, so a ruleset that
/// grants only read access leaves the card unusable — and the failure names a
/// missing device rather than a sandbox, which reads as "this machine has no GPU"
/// and sends the agent off to work around a problem it does not have. A ROCm
/// build dies on `/dev/kfd`, a CUDA one on `/dev/nvidiactl`.
///
/// Matched by name rather than a fixed list because the numbered nodes
/// (`nvidia0`, `nvidia1`, …) depend on how many cards are installed. Read live:
/// a readdir of `/dev` costs microseconds, and a cached answer that missed a
/// device after a driver reload would be a worse bug than the cost it saved.
#[cfg(target_os = "linux")]
pub(crate) fn gpu_device_nodes() -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut out: Vec<std::path::PathBuf> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            // `kfd` is AMD's compute node; `dri` the render/card directory every
            // vendor uses; `nvidia*` covers `nvidiactl`, `nvidia-uvm`,
            // `nvidia-caps` and the per-card numbers.
            name == "kfd" || name == "dri" || name.starts_with("nvidia")
        })
        .map(|e| e.path())
        .collect();
    // Stable order so the argv a test asserts on does not depend on readdir.
    out.sort();
    out
}

/// The note alone — the only thing any caller wants, now that there is exactly one
/// kind of denial to report.
///
/// There used to be a `DenialKind` beside it, so `shell` could decide what to *do*
/// about a failure: offer to re-run the command outside the sandbox, or not. With
/// escalation gone there is nothing to decide, and the network and ssh kinds went
/// with the confinements that caused them. Explaining the failure is the whole job.
pub fn sandbox_denial_note(policy: &SandboxPolicy, output: &str) -> Option<String> {
    if policy.mode == SandboxMode::None {
        return None;
    }
    let lower = output.to_ascii_lowercase();
    if !lower.contains("read-only file system") && !lower.contains("erofs") {
        return None;
    }
    let where_writable = if policy.writable_roots.is_empty() {
        "nothing is writable for this agent (read-only mode)".to_string()
    } else {
        format!(
            "writable here: {}{}",
            join_paths(&policy.project_writable_roots()),
            policy.cache_roots_clause(),
        )
    };
    Some(format!(
        "\n\n[sandbox] the \"read-only file system\" above is hrdr's sandbox refusing a write \
         outside this agent's roots — {where_writable}. The program is installed and working; it \
         tried to write somewhere it may not. If it was a package runner fetching a tool (`npx`, \
         `uvx`, `pipx`), run the copy already on PATH instead of downloading one. If the write is \
         genuinely needed, say so and name the directory — the user can allow it with \
         `sandbox_writable_roots` in the config or `--sandbox-writable-root <PATH>` on the \
         command line, and they can run the command themselves with `!<command>` — but do not \
         report the tool as missing or broken."
    ))
}

/// The command `shell`/`watch` actually spawn: `cmd_str` run through `shell`,
/// wrapped in whatever OS confinement the policy's mode demands. Mode `None`
/// — or a platform/kernel with no backend — returns exactly what
/// [`crate::Shell::command`] returns today, so the unsandboxed path is
/// byte-identical to the pre-sandbox behavior.
///
/// The caller still owns cwd, stdio, timeouts and process groups: every backend
/// passes them through untouched — no intermediate process mediates the
/// execution — and the existing group-kill still reaches every descendant.
///
/// `notices` is the **calling agent's** channel
/// ([`crate::ToolContext::sandbox_notices`]): every degradation this discovers is
/// owed to that agent and to no other.
pub fn sandboxed_shell_command(
    shell: crate::Shell,
    cmd_str: &str,
    policy: &SandboxPolicy,
    notices: &SandboxNotices,
) -> tokio::process::Command {
    if policy.mode == SandboxMode::None {
        return shell.command(cmd_str);
    }
    shell_command_with_backend(detect_backend(), shell, cmd_str, policy, notices)
}

/// [`sandboxed_shell_command`] with the backend chosen for it.
///
/// Split out so an arm is reachable on a machine whose detection would never pick
/// it — the Seatbelt arm on Linux, the Landlock arm on a kernel without the LSM —
/// which is otherwise code no test could execute.
///
/// An arm that ends up running a command with no confinement at all sets its
/// notice *first* — the one rule this layer may never break is pretending to
/// sandbox — and it sets it on the **calling agent's** `notices`, so a sibling
/// that never ran a shell command is not told its own sandbox degraded, and one
/// that did is not silenced by whoever got here first. Backend detection is cached
/// process-wide; the notice is not, so every command re-earns it.
///
/// `policy` is read only by the two real backends, and each is behind its own
/// `cfg` — so on a platform with neither (Windows) every use of it compiles out
/// and `-D warnings` calls the parameter unused. Waived rather than renamed:
/// `_policy` would read as "this function ignores the policy", which is false
/// everywhere it matters.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    allow(unused_variables)
)]
fn shell_command_with_backend(
    backend: OsSandboxBackend,
    shell: crate::Shell,
    cmd_str: &str,
    policy: &SandboxPolicy,
    notices: &SandboxNotices,
) -> tokio::process::Command {
    match backend {
        #[cfg(target_os = "linux")]
        OsSandboxBackend::Landlock => landlock_command(shell, cmd_str, policy),
        // `Landlock` is unreachable off Linux (detection never returns it),
        // but the variant exists on every platform, so the arm must too.
        #[cfg(not(target_os = "linux"))]
        OsSandboxBackend::Landlock => {
            notices.set(NO_OS_SANDBOX_NOTICE.to_string());
            shell.command(cmd_str)
        }
        #[cfg(target_os = "macos")]
        OsSandboxBackend::Seatbelt => {
            let mut cmd = tokio::process::Command::new(SEATBELT_PROGRAM);
            cmd.args(seatbelt_args(policy, shell, cmd_str));
            cmd
        }
        // The macOS twin of the Landlock arm above: unreachable off macOS,
        // still a variant that has to compile there.
        #[cfg(not(target_os = "macos"))]
        OsSandboxBackend::Seatbelt => {
            notices.set(NO_OS_SANDBOX_NOTICE.to_string());
            shell.command(cmd_str)
        }
        // Windows: only `Read` is confined so far. `Write` needs its roots
        // labelled Low before a Low-integrity child could write to them, so it
        // keeps the notice rather than getting a backend that would refuse every
        // write it is supposed to allow.
        #[cfg(windows)]
        OsSandboxBackend::LowIntegrity if policy.mode == SandboxMode::Read => {
            match std::env::current_exe() {
                Ok(exe) => {
                    let mut cmd = tokio::process::Command::new(exe);
                    cmd.args(low_integrity_args(shell, cmd_str));
                    cmd
                }
                Err(_) => {
                    notices.set(NO_OS_SANDBOX_NOTICE.to_string());
                    shell.command(cmd_str)
                }
            }
        }
        // The Landlock/Seatbelt twin: unreachable off Windows, still a variant
        // that has to compile there — and on Windows it is the not-yet-confined
        // `Write` path falling through to the same admission.
        OsSandboxBackend::LowIntegrity => {
            notices.set(NO_OS_SANDBOX_NOTICE.to_string());
            shell.command(cmd_str)
        }
        OsSandboxBackend::None => {
            notices.set(NO_OS_SANDBOX_NOTICE.to_string());
            shell.command(cmd_str)
        }
    }
}

/// The Landlock fallback: the shell is spawned exactly as it would be
/// unsandboxed, but the child confines *itself* between fork and exec, so the
/// ruleset covers the shell and every descendant it goes on to spawn.
///
/// One limit, decided and noticed: reads are unrestricted, because Landlock's
/// read axis cannot express "everything except…" without enumerating the
/// filesystem. That costs `Read` nothing (no writable roots IS the whole mode)
/// and costs `Strict` its read confinement for shell children, which
/// [`STRICT_DEGRADES_UNDER_LANDLOCK_NOTICE`] admits. A blocked write surfaces as
/// EACCES rather than EROFS.
///
/// In `Read` mode the policy's writable roots are empty, which is exactly
/// right here: the child may then write nowhere at all but `/dev/null`.
#[cfg(target_os = "linux")]
fn landlock_command(
    shell: crate::Shell,
    cmd_str: &str,
    policy: &SandboxPolicy,
) -> tokio::process::Command {
    let mut cmd = shell.command(cmd_str);
    // Resolved and cloned *before* the fork: a path that does not exist makes
    // `path_beneath_rules` fail the whole ruleset, and the closure must not go
    // touching the filesystem or the allocator's slow paths post-fork.
    let writable: Vec<PathBuf> = policy
        .writable_roots
        .iter()
        .filter(|root| root.exists())
        .cloned()
        .collect();
    // SAFETY: the closure runs in the forked child before `exec`. It issues
    // landlock/prctl syscalls and builds the ruleset from data moved in
    // beforehand; it shares no lock, handle, or global with the parent, and it
    // never spawns a thread.
    unsafe {
        cmd.pre_exec(move || install_landlock_rules(&writable));
    }
    cmd
}

/// Codex's `install_filesystem_landlock_rules_on_current_thread`
/// (`linux-sandbox/src/landlock.rs`) minus its seccomp: read everything, write
/// only `/dev/null` and the writable roots.
///
/// `BestEffort` compatibility means an older kernel silently enforces the
/// subset of ABI v5 it understands — but a kernel that enforces *nothing*
/// fails the spawn rather than running the command unconfined.
///
/// **No network handling at all**, deliberately: the sandbox confines the
/// filesystem and nothing else. `AccessNet` reached only TCP `bind`/`connect`
/// (ABI v4 added exactly two network rights and v5 adds none), so UDP — DNS and
/// QUIC/HTTP3 with it — raw sockets and anything already connected were outside
/// what it could express. A boundary that partial was a vestigial field rather
/// than a feature; if network confinement returns it needs a real threat model.
#[cfg(target_os = "linux")]
fn install_landlock_rules(writable_roots: &[PathBuf]) -> std::io::Result<()> {
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, path_beneath_rules,
    };

    let abi = ABI::V5;
    let access_rw = AccessFs::from_all(abi);
    let access_ro = AccessFs::from_read(abi);

    let base = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(access_rw)
        .map_err(std::io::Error::other)?;

    let mut ruleset = base
        .create()
        .map_err(std::io::Error::other)?
        .add_rules(path_beneath_rules(["/"], access_ro))
        .map_err(std::io::Error::other)?
        .add_rules(path_beneath_rules(["/dev/null"], access_rw))
        .map_err(std::io::Error::other)?
        // A GPU node is opened read-write to submit work at all, so read access
        // alone leaves it unusable: the nodes are visible, but the ruleset would
        // otherwise deny the open. See `gpu_device_nodes`.
        .add_rules(path_beneath_rules(gpu_device_nodes(), access_rw))
        .map_err(std::io::Error::other)?
        // Codex calls this `set_no_new_privs`, deprecated since its pin.
        .no_new_privs(true);

    if !writable_roots.is_empty() {
        ruleset = ruleset
            .add_rules(path_beneath_rules(writable_roots, access_rw))
            .map_err(std::io::Error::other)?;
    }
    let status = ruleset.restrict_self().map_err(std::io::Error::other)?;
    if status.ruleset == RulesetStatus::NotEnforced {
        // Half-confined is not confined: refuse the spawn instead.
        return Err(std::io::Error::other("landlock not enforced"));
    }
    Ok(())
}

/// macOS's sandbox wrapper, by absolute path.
///
/// Pinned rather than looked up on `PATH` — exactly as Codex does
/// (`MACOS_PATH_TO_SEATBELT_EXECUTABLE`) — so a poisoned `PATH` cannot swap the
/// confinement for a same-named no-op. If `/usr/bin/sandbox-exec` itself has
/// been tampered with, whoever did it already had root.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SEATBELT_PROGRAM: &str = "/usr/bin/sandbox-exec";

/// The full `sandbox-exec` argv (everything after `argv[0]`): the generated
/// profile, then the shell invocation it applies to.
///
/// There is no `--chdir` to pass — Seatbelt only filters syscalls, so the child
/// inherits the cwd the caller sets on the `Command`, and stdio, exit status,
/// timeouts and group-kill are untouched.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// The hidden argv[1] that puts hrdr into its own confinement wrapper.
///
/// Windows has no `pre_exec`, and a `tokio::process::Command` cannot carry a
/// custom token, so there is nowhere in the spawn seam to hand the kernel a
/// lowered token. Re-execing ourselves is what closes that gap: the wrapper
/// lowers ITS token — permitted, because only raising integrity is refused —
/// and every descendant inherits the result. Same shape as `sandbox-exec -p
/// <profile> -- <shell> -c <cmd>`, with hrdr playing `sandbox-exec`.
///
/// Double-underscored and undocumented: it is an implementation detail of the
/// Windows backend, not a command anyone should type.
pub const SANDBOX_EXEC_ARG: &str = "__sandbox-exec";

/// The full argv for the Low-integrity wrapper: `__sandbox-exec -- <shell>
/// <invoke args> <cmd>`, to be passed to our own executable.
///
/// No writable roots are threaded through yet. `Read` (and `Jail`, which holds
/// no shell at all) is the whole of what this delivers: every write refused,
/// reads untouched. `Write` still needs its roots labelled Low before a
/// confined child could write to them, which is a filesystem mutation and a
/// separate slice — until then `Write` keeps the software path-guard and the
/// no-OS-sandbox notice, exactly as it did before this backend existed.
#[cfg_attr(not(windows), allow(dead_code))]
fn low_integrity_args(shell: crate::Shell, cmd_str: &str) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> =
        vec![SANDBOX_EXEC_ARG.into(), "--".into(), shell.program().into()];
    args.extend(shell.invoke_args().iter().map(std::ffi::OsString::from));
    args.push(cmd_str.into());
    args
}

/// Lower this process's token to Low integrity, permanently and irreversibly.
///
/// Called by the [`SANDBOX_EXEC_ARG`] wrapper on itself before it spawns the
/// real shell. After this returns, the kernel refuses every write to an object
/// labelled Medium or higher; reads are unaffected.
///
/// # Errors
///
/// Any failure must be fatal to the caller: a wrapper that could not lower
/// itself and ran the command anyway would execute it unconfined while the
/// backend reported as active, which is worse than having no backend.
#[cfg(windows)]
pub fn lower_current_process_to_low_integrity() -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_MANDATORY_LABEL,
        TokenIntegrityLevel,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// `SE_GROUP_INTEGRITY`, spelled out for the same reason the SID below is:
    /// `windows-sys` 0.52 does not export it from `Win32::Security`, and which
    /// module a constant lives in has moved between releases. The value is fixed
    /// by the ABI — it marks the SID in a `TOKEN_MANDATORY_LABEL` as the token's
    /// integrity label rather than an ordinary group.
    const SE_GROUP_INTEGRITY: u32 = 0x0000_0020;

    // S-1-16-4096 built inline rather than via `ConvertStringSidToSidW`: that
    // returns a `LocalFree`-owned allocation, and both the handle type and the
    // module `LocalFree` lives in have moved between `windows-sys` releases. A
    // literal SID needs no allocator and no second feature, and its layout is
    // fixed by the SID format: revision, sub-authority count, the six-byte
    // authority (SECURITY_MANDATORY_LABEL_AUTHORITY), then one little-endian
    // sub-authority (SECURITY_MANDATORY_LOW_RID = 0x1000).
    #[repr(C, align(4))]
    struct LowIntegritySid([u8; 12]);
    let mut low_sid = LowIntegritySid([1, 1, 0, 0, 0, 0, 0, 16, 0x00, 0x10, 0x00, 0x00]);

    // SAFETY: `low_sid` is a stack local of exactly the SID layout the call
    // expects and outlives the call; `token` is checked before use and closed on
    // every path. Nothing here allocates, so there is nothing to leak.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_DEFAULT, &mut token) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: std::ptr::addr_of_mut!(low_sid).cast(),
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        let ok = SetTokenInformation(
            token,
            TokenIntegrityLevel,
            std::ptr::addr_of_mut!(label).cast(),
            (std::mem::size_of::<TOKEN_MANDATORY_LABEL>() + std::mem::size_of::<LowIntegritySid>())
                as u32,
        );
        let err = (ok == 0).then(std::io::Error::last_os_error);
        CloseHandle(token);
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn seatbelt_args(
    policy: &SandboxPolicy,
    shell: crate::Shell,
    cmd_str: &str,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "-p".into(),
        seatbelt_profile(policy.mode, policy).into(),
        "--".into(),
        shell.program().into(),
    ];
    args.extend(shell.invoke_args().iter().map(std::ffi::OsString::from));
    args.push(cmd_str.into());
    args
}

/// The SBPL profile for `mode` (§4 slice 8, verbatim): deny by default, allow
/// the process machinery a shell needs, then open exactly the file access the
/// mode grants.
///
/// `Write` reads everywhere and writes only under the policy's writable roots;
/// `Read` narrows reads to the system directories an interpreter or compiler
/// needs plus the readable roots, and grants no writes at all. The network is
/// always allowed: no mode confines it.
///
/// **Caveat carried from the spec:** the `Read` variant is author-written and
/// **unvalidated** — no Mac was available when this landed, so it has never
/// been run. The `Write` variant is also a coarsening of Codex's
/// `seatbelt_base_policy.sbpl` (broad `sysctl-read`/`mach-lookup`/`ipc-posix*`
/// in place of its enumerated names, and none of its `pseudo-tty`/`iokit-open`/
/// `user-preference-read` allowances), so real-world macOS runs may need
/// additions here.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn seatbelt_profile(mode: SandboxMode, policy: &SandboxPolicy) -> String {
    /// `(subpath "…")` per root, space-separated — SBPL's "this directory and
    /// everything beneath it" filter.
    ///
    /// A path is a quoted SBPL string, so a literal backslash or quote in it
    /// must be escaped or the profile either changes meaning or fails to
    /// parse (and a profile that fails to parse is a command that does not
    /// run — never a boundary that silently widens).
    fn subpaths(roots: &[PathBuf]) -> String {
        roots
            .iter()
            .map(|root| {
                let escaped = root
                    .to_string_lossy()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"");
                format!("(subpath \"{escaped}\")")
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The public spelling of a canonicalized root: `/private/var/…` →
    /// `/var/…`. [`SandboxPolicy`] keeps only the canonical form (that is what
    /// makes the file-tool guard escape-proof), but Seatbelt matches the
    /// pathname a process passes to the syscall — which on macOS is the
    /// symlinked `/var/folders/…`, not the `/private/var/folders/…` that
    /// `canonicalize` resolves it to. A profile that grants only the canonical
    /// form denies every write through the public one. No-op for roots without
    /// the `/private/` prefix (the common case: a cwd or any path with no
    /// symlinked ancestor).
    fn literal_siblings(roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for root in roots {
            let s = root.to_string_lossy();
            let Some(literal) = s.strip_prefix("/private") else {
                continue;
            };
            let candidate = PathBuf::from(literal);
            if !out.contains(&candidate) && !roots.contains(&candidate) {
                out.push(candidate);
            }
        }
        out
    }

    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         (allow process-fork)\n\
         (allow process-exec*)\n\
         (allow signal)\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow ipc-posix*)\n",
    );
    match mode {
        SandboxMode::Write | SandboxMode::Read => {
            profile.push_str("(allow file-read*)\n");
            // The standard devices stay open for writing even though they are
            // not writable roots: `2>/dev/null` is the one write every shell
            // command needs, and without it a sandboxed command fails with
            // "bash: /dev/null: Operation not permitted" — the exact error a
            // watch check (`cat … 2>/dev/null || …`) hit on macOS CI before
            // the roots line was ever reached. /dev/null discards, the rest
            // are generators; every real-world Seatbelt profile allows them.
            profile.push_str(
                "(allow file-write* (subpath \"/dev/null\") (subpath \"/dev/zero\") \
                 (subpath \"/dev/random\") (subpath \"/dev/urandom\"))\n",
            );
            // An empty root list must stay closed: `(allow file-write*)` with
            // no filter allows every write there is, so the line is omitted
            // and `(deny default)` answers instead. `Read` has no writable
            // roots at all, so it always takes that branch — broad reads, no
            // project writes anywhere (the device line above is the only
            // exception).
            if !policy.writable_roots.is_empty() {
                // Grant the literal spellings too: the roots are canonicalized
                // (macOS resolves `/var` to `/private/var`), but Seatbelt
                // matches the pathname a process passes to the syscall —
                // `/var/folders/…` — which a `(subpath "/private/var/…")`
                // filter does not cover. Both name the same directory.
                let mut roots = policy.writable_roots.clone();
                roots.extend(literal_siblings(&policy.writable_roots));
                let writes = subpaths(&roots);
                profile.push_str(&format!("(allow file-write* {writes})\n"));
            }
        }
        SandboxMode::Jail => {
            let reads = subpaths(&policy.readable_roots);
            let reads = if reads.is_empty() {
                String::new()
            } else {
                format!(" {reads}")
            };
            profile.push_str(&format!(
                "(allow file-read* (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\")\n  \
                 (subpath \"/System\") (subpath \"/Library\") (subpath \"/private/etc\")\n  \
                 (subpath \"/dev\"){reads})\n"
            ));
        }
        // Unreachable: `sandboxed_shell_command` returns before it gets here.
        SandboxMode::None => {}
    }
    // Omission IS the denial here: the profile opens with `(deny default)`, so
    // an operation nothing allows is already refused, and an explicit
    // `(deny network*)` would add a line that changes no decision. Unlike the
    // `.git` case above there is no earlier `allow` to subtract from — that one
    // needs its trailing `deny` precisely because SBPL is last-match-wins and
    // `(allow file-write* …)` came first.
    // The network is never confined, in any mode — so this is unconditional
    // rather than a policy question.
    profile.push_str("(allow network*)\n");
    profile
}

/// A [`crate::ToolContext`] rooted at `dir` and confined to it *alone*, for
/// the file-tool guard tests.
///
/// Deliberately a struct literal rather than [`SandboxPolicy::for_agent`]:
/// `for_agent` makes [`std::env::temp_dir`] writable, so a second tempdir
/// would sit *inside* the roots and no "outside" assertion could ever fire.
#[cfg(test)]
pub(crate) fn confined_ctx(dir: &Path, mode: SandboxMode) -> crate::ToolContext {
    let root = canonicalize_nearest(dir);
    let mut ctx = crate::ToolContext::new(dir.to_path_buf());
    ctx.sandbox = std::sync::Arc::new(SandboxPolicy {
        mode,
        writable_roots: vec![root.clone()],
        readable_roots: vec![root],
        cache_roots: Vec::new(),
        wrap_tool_results: false,
    });
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults have to make `cargo build` and `npm i` work with no
    /// configuration, which is the whole point of granting them: config and
    /// `--sandbox-writable-root` are the escape hatch for a bespoke layout, not
    /// the mechanism by which mainstream tooling becomes usable.
    ///
    /// Cargo's own caches are the subject because they are the verified failure:
    /// a build under cwd-only confinement downloads the crate successfully and
    /// then dies writing it into `$CARGO_HOME/registry/cache`.
    #[test]
    fn write_mode_grants_the_package_caches() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        let cargo_home = tool_home("CARGO_HOME", ".cargo", home_dir().as_deref())
            .expect("a home or an override");
        if !cargo_home.is_dir() {
            return; // no cargo on this machine — nothing to grant
        }

        for cache in [cargo_home.join("registry"), cargo_home.join("git")] {
            assert!(cache.is_dir(), "{} was not created", cache.display());
            let probe = cache.join("probe");
            policy
                .check_write(&canonicalize_nearest(&probe), &probe)
                .unwrap_or_else(|e| panic!("{} must be writable: {e}", cache.display()));
            assert!(
                policy
                    .cache_roots
                    .iter()
                    .any(|c| c == &canonicalize_nearest(&cache)),
                "a cache root must be LABELLED as one, or the prompt lists it"
            );
        }

        // A binary directory is NOT granted: a binary on PATH is a persistence
        // vector — the next command the *user* runs could be the agent's — so
        // `cargo install` fails by default.
        let bin = cargo_home.join("bin").join("malware");
        assert!(
            policy
                .check_write(&canonicalize_nearest(&bin), &bin)
                .is_err(),
            "a directory on PATH must not be writable"
        );
        // Nor is the tool home itself, which is where credentials live.
        let creds = cargo_home.join("credentials.toml");
        assert!(
            policy
                .check_write(&canonicalize_nearest(&creds), &creds)
                .is_err(),
            "granting a cache must not grant its parent"
        );
    }

    /// The caches are enforcement, not narration: they are in `writable_roots`
    /// (which is all the OS layer reads) and out of what a prompt or a refusal
    /// names, because the model never chooses to write there — `cargo` does.
    #[test]
    fn the_caches_are_enforced_but_not_narrated() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        assert!(!policy.cache_roots.is_empty(), "some cache was granted");
        for cache in &policy.cache_roots {
            assert!(
                policy.writable_roots.contains(cache),
                "{} must be enforced, not only labelled",
                cache.display()
            );
        }
        let named = policy.project_writable_roots();
        for cache in &policy.cache_roots {
            assert!(
                !named.contains(&cache.as_path()),
                "{} must not be listed to the model",
                cache.display()
            );
        }
        assert!(
            named.iter().any(|n| *n == canonicalize_nearest(dir.path())),
            "the cwd is still named: {named:?}"
        );
        assert!(
            policy.cache_roots_clause().contains("package-manager"),
            "the omission is summarized in one clause"
        );

        // Read mode grants nothing at all, caches included.
        let read = SandboxPolicy::for_agent(SandboxMode::Read, dir.path(), &[]);
        assert!(read.cache_roots.is_empty() && read.writable_roots.is_empty());
    }

    /// A cache whose location is overridden by an env var must be resolved from
    /// that var. A hardcoded `~/.cargo/registry` on a machine with
    /// `CARGO_HOME=/opt/cargo` grants nothing, and the build then fails with
    /// exactly the confusing EROFS the grant exists to prevent.
    ///
    /// Reads the resolver directly rather than mutating the process environment:
    /// `set_var` is unsound once the test harness has threads.
    #[test]
    fn an_env_override_decides_where_a_cache_lives() {
        let home = home_dir().expect("the test sandbox set $HOME");
        assert_eq!(
            tool_home("HRDR_NO_SUCH_VAR", ".cargo", Some(&home)),
            Some(home.join(".cargo")),
            "an unset var falls back to the home-relative default"
        );
        // `$HOME` itself is a set, absolute var — enough to prove the override
        // wins over the fallback without touching the environment.
        assert_eq!(
            tool_home("HOME", ".cargo", Some(&home)),
            Some(home.clone()),
            "a set override wins outright"
        );
        // Relative values are ignored: they would resolve against whatever cwd
        // this process happens to have, not the one the tool will use.
        assert_eq!(env_dir("PATH").map(|p| p.is_absolute()), Some(true));
    }

    /// Creation completes an existing layout; it does not invent one. `~/.cargo`
    /// exists exactly when cargo is installed, so `~/.cargo/registry` is created
    /// on a machine that builds Rust and skipped on one that never will —
    /// otherwise hrdr would scatter two dozen empty directories through the home
    /// of anyone who runs it once.
    #[test]
    fn a_cache_root_is_only_created_inside_a_layout_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("tool-home").join("cache");
        assert!(
            !ensure_cache_root(&nested),
            "no tool home, no grant: {}",
            nested.display()
        );
        assert!(!nested.exists(), "and nothing was created");

        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        assert!(ensure_cache_root(&nested), "the layout now exists");
        assert!(nested.is_dir(), "so the cache was created");

        // Idempotent, and a directory that is already there is simply granted.
        assert!(ensure_cache_root(&nested));
    }

    /// A blocked write is reported as the SANDBOX, not as a broken tool.
    ///
    /// Verbatim from a real run: the model wrote a report, then ran
    /// `npx prettier --write docs/code-review.md`. `prettier` was installed and
    /// on `PATH`, but `npx` ignored it and tried to fetch the package into
    /// `~/.npm/_cacache`, which the sandbox binds read-only. The model read the
    /// `EROFS` as "prettier is not available in this environment" — a false claim
    /// about the machine — and skipped formatting.
    #[test]
    fn a_sandboxed_write_denial_is_named_as_the_sandbox() {
        const NPX_EROFS: &str = "npm error code EROFS\n\
             npm error syscall open\n\
             npm error path /home/u/.npm/_cacache/tmp/aa7100ad\n\
             npm error errno EROFS\n\
             npm error rofs Invalid response body while trying to fetch \
             https://registry.npmjs.org/prettier: EROFS: read-only file system";

        let dir = tempfile::tempdir().unwrap();
        let write = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        let note = sandbox_denial_note(&write, NPX_EROFS).expect("the denial is recognized");
        assert!(note.contains("[sandbox]"), "{note}");
        assert!(note.contains("writable here:"), "{note}");
        // It must say the thing the model got wrong, in as many words.
        assert!(
            note.contains("do not report the tool as missing or broken"),
            "{note}"
        );
        assert!(note.contains("run the copy already on PATH"), "{note}");

        // A read-mode agent has no writable root at all — say that, rather than
        // printing an empty list.
        let read = SandboxPolicy::for_agent(SandboxMode::Read, dir.path(), &[]);
        let ro_note = sandbox_denial_note(&read, NPX_EROFS).expect("recognized in read mode too");
        assert!(ro_note.contains("read-only mode"), "{ro_note}");

        // Unconfined: the sandbox did not do this, so it says nothing.
        assert_eq!(
            sandbox_denial_note(&SandboxPolicy::unconfined(), NPX_EROFS),
            None
        );
        // …and NARROW: an ordinary failure is never editorialized over. A bare
        // "Permission denied" is a normal error a program raises for its own
        // reasons, and annotating it would be noise on every one of them.
        for ordinary in [
            "error: could not compile `foo`",
            "cat: /etc/shadow: Permission denied",
            "fatal: not a git repository",
            "",
        ] {
            assert_eq!(sandbox_denial_note(&write, ordinary), None, "{ordinary}");
        }
    }

    /// **A refused write is the only thing the sandbox explains, and it names the
    /// remedy.** Everything else that used to get a note — a resolver failure, an
    /// ssh ownership complaint, a GPU node missing under jail — belonged to a
    /// confinement that no longer exists, and a note asserting the sandbox over a
    /// real local problem is worse than no note at all.
    #[test]
    fn only_a_refused_write_is_explained_and_the_remedy_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let write = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);

        let note = sandbox_denial_note(&write, "EROFS: read-only file system").expect("explained");
        assert!(note.contains("[sandbox]"), "{note}");
        // An error that explains the cause and withholds the fix is half an error.
        assert!(note.contains("--sandbox-writable-root"), "{note}");
        assert!(note.contains("sandbox_writable_roots"), "{note}");
        assert!(note.contains("!<command>"), "{note}");

        for foreign in [
            "curl: (6) Could not resolve host: example.com",
            "Bad owner or permissions on /etc/ssh/ssh_config",
            "hipErrorNoDevice: failed to open /dev/kfd",
        ] {
            assert_eq!(sandbox_denial_note(&write, foreign), None, "{foreign}");
        }
        // …in every mode, including the one that used to claim the GPU case.
        let jail = SandboxPolicy::for_agent(SandboxMode::Jail, dir.path(), &[]);
        assert_eq!(
            sandbox_denial_note(&jail, "hipErrorNoDevice: failed to open /dev/kfd"),
            None
        );
    }

    /// **A write agent scoped to a subdirectory can still commit.**
    ///
    /// `task`'s `cwd` argument introduced the case: narrow a write sub-agent to
    /// `crates/foo` and the repository's `.git` sits *above* its only writable root,
    /// so `git add`/`commit` die on an EROFS deep inside git about a path nobody
    /// mentioned. [`enclosing_git_dir`] grants exactly that and nothing wider.
    ///
    /// Asserted on the resolver rather than through a `for_agent` policy, because
    /// every path a test can build lives under `env::temp_dir()` — itself a writable
    /// root — so the interesting grant would be swallowed as redundant and the
    /// interesting refusal would pass for the wrong reason.
    #[test]
    fn a_write_agent_scoped_below_the_repo_root_can_still_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = canonicalize_nearest(dir.path());
        std::fs::create_dir_all(repo.join(".git").join("refs")).unwrap();
        let scoped = repo.join("crates").join("foo");
        std::fs::create_dir_all(scoped.join("src")).unwrap();

        // Scoped below the root: the repo's metadata is granted.
        assert_eq!(
            enclosing_git_dir(&scoped),
            Some(repo.join(".git")),
            "a scoped write agent must be able to commit"
        );
        // At the root: `.git` is already under the cwd, so nothing is added — a
        // redundant root would only make the refusal message longer.
        assert_eq!(enclosing_git_dir(&repo), None);
        // In no repository at all: nothing to grant.
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(enclosing_git_dir(&canonicalize_nearest(bare.path())), None);

        // A linked worktree's `.git` is a FILE, and following it is
        // `git_metadata_roots`'s job — with a much narrower grant than the whole
        // gitdir. This resolver must not hand over the parent's metadata wholesale.
        let wt = tempfile::tempdir().unwrap();
        let wt = canonicalize_nearest(wt.path());
        std::fs::write(wt.join(".git"), "gitdir: /elsewhere/.git/worktrees/wt\n").unwrap();
        let inside = wt.join("crates");
        std::fs::create_dir_all(&inside).unwrap();
        assert_eq!(enclosing_git_dir(&inside), None);

        // And the policy really does carry it: `check_write` on the metadata passes
        // for an agent whose cwd is the narrow one.
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: canonical_roots(
                std::iter::once(scoped.clone())
                    .chain(enclosing_git_dir(&scoped))
                    .collect(),
            ),
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };
        for path in [
            scoped.join("src").join("lib.rs"),
            repo.join(".git").join("index"),
            repo.join(".git").join("refs").join("heads").join("main"),
        ] {
            policy
                .check_write(&canonicalize_nearest(&path), &path)
                .unwrap_or_else(|e| panic!("{} must be writable: {e}", path.display()));
        }
        // A sibling crate is not: the grant is the metadata, not the repository.
        let sibling = repo.join("crates").join("bar").join("src").join("lib.rs");
        assert!(
            policy
                .check_write(&canonicalize_nearest(&sibling), &sibling)
                .is_err(),
            "scoping must still mean something"
        );
    }

    /// A git-metadata write is an ORDINARY write now: the `.git` lock is gone, so
    /// a repo under a writable root takes commits from any agent, main or
    /// delegated. Pinned because the failure mode of a re-introduced lock is
    /// silent — a sub-agent told to commit its own work simply cannot.
    #[test]
    fn git_metadata_is_writable_like_any_other_path() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join(".git");
        std::fs::create_dir_all(repo.join("refs").join("heads")).unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        for path in [
            repo.join("index"),
            repo.join("refs").join("heads").join("main"),
            repo.join("hooks").join("pre-commit"),
            repo.join("config"),
        ] {
            let canon = canonicalize_nearest(&path);
            policy
                .check_write(&canon, &path)
                .unwrap_or_else(|e| panic!("{} must be writable: {e}", path.display()));
        }
    }

    /// **No mode confines the network, so no network failure is ever the
    /// sandbox's.** A resolver timeout or an unreachable route is what a machine
    /// with a dead link looks like, and claiming the sandbox did it would send the
    /// model debugging DNS in the one direction that cannot help.
    ///
    /// The confinement was deleted rather than kept, because in the mode that
    /// mattered it was never a boundary: a delegated agent reports to an agent that
    /// *does* have a network, so injected text reaching a sub-agent propagates to
    /// the parent through its report and the parent can curl. It bought one hop of
    /// latency, not containment.
    #[test]
    fn a_network_failure_is_never_attributed_to_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [SandboxMode::Write, SandboxMode::Read, SandboxMode::Jail] {
            let policy = SandboxPolicy::for_agent(mode, dir.path(), &[]);
            for failure in [
                "curl: (6) Could not resolve host: api.example.com",
                "fatal: unable to access 'https://github.com/o/r/': Could not resolve host",
                "pip install foo\nTemporary failure in name resolution",
                "ping: connect: Network is unreachable",
            ] {
                assert_eq!(
                    sandbox_denial_note(&policy, failure),
                    None,
                    "{mode:?} does not confine the network: {failure}"
                );
            }
        }
    }

    /// `check_write` with the canonicalization its callers owe it.
    fn check_write(policy: &SandboxPolicy, path: &Path) -> anyhow::Result<()> {
        policy.check_write(&canonicalize_nearest(path), path)
    }

    /// `check_read` with the canonicalization its callers owe it.
    fn check_read(policy: &SandboxPolicy, path: &Path) -> anyhow::Result<()> {
        policy.check_read(&canonicalize_nearest(path), path)
    }

    #[test]
    fn sandbox_mode_parses_all_spellings_and_rejects_garbage() {
        assert_eq!("write".parse::<SandboxMode>().unwrap(), SandboxMode::Write);
        assert_eq!("READ".parse::<SandboxMode>().unwrap(), SandboxMode::Read);
        assert_eq!("  none ".parse::<SandboxMode>().unwrap(), SandboxMode::None);
        assert_eq!(SandboxMode::Write.to_string(), "write");
        assert_eq!(SandboxMode::Read.to_string(), "read");
        assert_eq!(SandboxMode::None.to_string(), "none");

        let err = "wrote".parse::<SandboxMode>().unwrap_err();
        assert!(err.contains("wrote"), "{err}");
        for valid in ["write", "read", "none"] {
            assert!(err.contains(valid), "{err} should name {valid}");
        }
    }

    #[test]
    fn session_scratch_dir_is_private_stable_and_under_temp() {
        let first = session_scratch_dir();
        let second = session_scratch_dir();
        assert_eq!(first, second, "the scratch dir is per-process and cached");
        assert!(first.is_dir(), "{} should exist", first.display());
        assert!(
            first.starts_with(std::env::temp_dir()),
            "{} should live under the temp dir",
            first.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(first).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "scratch dir must be owner-only");
        }
    }

    #[test]
    fn policy_write_roots_cover_cwd_temp_scratch_and_tool_output() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);

        check_write(&policy, &dir.path().join("out.txt")).unwrap();
        check_write(&policy, &std::env::temp_dir().join("hrdr-write-probe")).unwrap();
        check_write(&policy, &session_scratch_dir().join("probe")).unwrap();
        check_write(&policy, &tool_output_dir().join("probe")).unwrap();

        // The temp dir is a writable root by design, so a *sibling* of the
        // test cwd is allowed — only paths outside the temp tree are refused.
        let sibling = dir.path().parent().unwrap().join("hrdr-sibling-probe");
        check_write(&policy, &sibling).unwrap();

        let err = check_write(&policy, Path::new("/etc/passwd"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to write /etc/passwd"), "{err}");
        assert!(err.contains("You may write only under"), "{err}");
        for root in policy.project_writable_roots() {
            assert!(
                err.contains(&root.display().to_string()),
                "{err} should name {root:?}"
            );
        }
        check_write(&policy, Path::new("/nonexistent-outside/f")).unwrap_err();
    }

    #[test]
    fn strict_mode_refuses_reads_outside_roots_and_allows_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Jail, dir.path(), &[]);

        check_read(&policy, &dir.path().join("notes.md")).unwrap();
        check_read(&policy, &session_scratch_dir().join("probe")).unwrap();
        check_read(&policy, &tool_output_dir().join("probe")).unwrap();

        let err = check_read(&policy, Path::new("/etc/passwd"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to read /etc/passwd"), "{err}");
        assert!(err.contains("strictly confined and may read only"), "{err}");
        assert!(
            err.contains(&canonicalize_nearest(dir.path()).display().to_string()),
            "{err}"
        );
        // Read mode writes nothing anywhere.
        check_write(&policy, &dir.path().join("out.txt")).unwrap_err();

        // `check_read` is a no-op in the other two modes.
        let write_mode = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        check_read(&write_mode, Path::new("/etc/passwd")).unwrap();
        check_read(&SandboxPolicy::unconfined(), Path::new("/etc/passwd")).unwrap();
        check_write(&SandboxPolicy::unconfined(), Path::new("/etc/passwd")).unwrap();
    }

    /// `allow_read` widens jail's read boundary and nothing else — the case it
    /// exists for is hrdr's Agent Skill roots, which sit outside the working tree
    /// and must stay readable in the one mode that confines reads.
    #[test]
    fn allow_read_widens_jail_reads_only() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let skills = elsewhere.path().join(".claude").join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let missing = elsewhere.path().join("never-created");

        let mut policy = SandboxPolicy::for_agent(SandboxMode::Jail, dir.path(), &[]);
        // Refused before the grant — otherwise this test would pass on a policy
        // that never read the roots at all.
        check_read(&policy, &skills.join("ship/SKILL.md")).unwrap_err();

        policy.allow_read(vec![skills.clone(), missing.clone()]);
        check_read(&policy, &skills.join("ship/SKILL.md")).unwrap();
        // A root nobody created is not carried, so it never reaches the prompt's
        // "you may read only under" list.
        assert!(
            !policy
                .readable_roots
                .iter()
                .any(|r| r == &canonicalize_nearest(&missing)),
            "{:?}",
            policy.readable_roots
        );
        // Reads only: the grant opens no write anywhere, jail's roots included.
        check_write(&policy, &skills.join("x")).unwrap_err();
        check_write(&policy, &dir.path().join("x")).unwrap_err();

        // Unconfined stays byte-identical to `unconfined()` — mode None answers
        // every question with "allowed" and must carry no roots to render.
        let mut none = SandboxPolicy::for_agent(SandboxMode::None, dir.path(), &[]);
        none.allow_read(vec![skills]);
        assert!(none.readable_roots.is_empty());
    }

    #[test]
    fn symlink_and_dotdot_escapes_are_caught() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);

        // Enough `..` to bottom out at `/` no matter how deep the temp dir is.
        let escape = dir
            .path()
            .join("a")
            .join(format!("{}etc/passwd", "../".repeat(40)));
        check_write(&policy, &escape).unwrap_err();

        // The symlink target must sit outside the temp tree: another tempdir
        // would be under the writable `env::temp_dir()` root and allowed.
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink("/etc", &link).unwrap();
            check_write(&policy, &link.join("passwd")).unwrap_err();
        }
    }

    /// A dangling symlink inside a writable root must not smuggle a write out:
    /// the write tool follows the link and creates the file at the target, so
    /// the guard has to resolve the link *before* the root check. Regression
    /// for the escape where `canonicalize_nearest` kept the lexical link path
    /// (inside the root) and the write landed outside it. Unix-only — creating
    /// symlinks on Windows needs privileges.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_cannot_escape_the_writable_roots() {
        let dir = tempfile::tempdir().unwrap();
        // Canonical root so the symlink target compares against the same
        // resolved ancestor `canonicalize_nearest` produces on macOS and
        // Windows. No-op on Linux.
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![root.clone()],
            readable_roots: vec![root.clone()],
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };

        // `root/link -> ../outside.txt`: parent exists, target file does not —
        // the dangling case the guard used to wave through. A write through it
        // creates the file outside the root.
        let link = root.join("link");
        std::os::unix::fs::symlink("../outside.txt", &link).unwrap();
        assert!(
            policy
                .check_write(&canonicalize_nearest(&link), &link)
                .is_err(),
            "a dangling symlink must not write outside the root"
        );

        // The benign case: a dangling link pointing at a not-yet-existing file
        // inside the root stays writable.
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let benign = inner.join("link");
        std::os::unix::fs::symlink("new.txt", &benign).unwrap();
        assert!(
            policy
                .check_write(&canonicalize_nearest(&benign), &benign)
                .is_ok(),
            "a dangling symlink inside the root must stay writable"
        );
    }

    /// A real repo with one commit at `<root>/repo` plus a linked worktree at
    /// `<root>/wt` on branch `hrdr/task-1` — the exact shape a write
    /// sub-agent runs in, and the one the metadata roots exist for.
    fn repo_with_linked_worktree(root: &Path) -> (PathBuf, PathBuf) {
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        let wt = root.join("wt");
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "hrdr/task-1",
            wt.to_str().unwrap(),
        ]);
        (repo, wt)
    }

    #[test]
    fn git_metadata_roots_for_a_linked_worktree() {
        if which::which("git").is_err() {
            return; // best-effort: exercise the real backend when available
        }
        let dir = tempfile::tempdir().unwrap();
        let (repo, wt) = repo_with_linked_worktree(dir.path());

        // A plain checkout needs nothing extra: its `.git` is under the cwd.
        assert!(git_metadata_roots(&repo).is_empty());

        let roots = git_metadata_roots(&wt);
        assert_eq!(roots.len(), 4, "{roots:?}");
        let common = canonicalize_nearest(&repo.join(".git"));
        // Both sides through `canonicalize_nearest`: macOS tempdirs live
        // behind the `/var → /private/var` symlink.
        let expected = [
            canonicalize_nearest(&common.join("worktrees").join("wt")),
            canonicalize_nearest(&common.join("objects")),
            canonicalize_nearest(&common.join("refs").join("heads").join("hrdr")),
            canonicalize_nearest(&common.join("logs").join("refs").join("heads").join("hrdr")),
        ];
        let got: Vec<PathBuf> = roots.iter().map(|r| canonicalize_nearest(r)).collect();
        assert_eq!(got, expected.to_vec());
        for root in &got {
            assert!(root.is_dir(), "{} should exist", root.display());
        }
        // Narrow, and that is the point: the parent repo's own `index`, `config`
        // and other branches' refs are NOT granted. The grant exists so a
        // worktree can commit, not so it can rewrite the repository it hangs off.
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: canonical_roots(
                std::iter::once(wt.clone()).chain(roots).collect::<Vec<_>>(),
            ),
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };
        check_write(&policy, &common.join("index")).unwrap_err();
        check_write(&policy, &common.join("refs").join("heads").join("main")).unwrap_err();
        // What IS granted is the append-only object store — bind it read-only and
        // no commit from the worktree can complete.
        check_write(&policy, &common.join("objects").join("aa").join("bb")).unwrap();
    }

    /// "Is it under a writable root" is the ONLY question a write has to answer.
    ///
    /// There used to be a second one: a `.git` component anywhere in the
    /// canonical path was refused to the file tools, on the theory that
    /// `.git/hooks/pre-commit` is a file the user's next commit executes. It is
    /// deleted, and this pins the deletion — `shell` reached every one of those
    /// paths regardless (`git config`, a heredoc, `printf >`), so the guard
    /// stopped the honest path and nothing else, while refusing legitimate
    /// `.git/info/exclude` edits and the hooks a user had asked for. Oversight of
    /// git belongs at the shell layer, where guardrails run.
    #[test]
    fn a_writable_root_is_writable_all_the_way_down() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonicalize_nearest(dir.path());
        // Struct literal, not `for_agent`: the subject is paths *inside* the
        // root, and a writable `env::temp_dir()` root only adds noise.
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![root.clone()],
            readable_roots: vec![root],
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };

        for allowed in [
            ".git/hooks/pre-commit",
            ".git/config",
            ".git/info/exclude",
            "vendor/dep/.git/hooks/post-checkout",
            ".hrdr/commands/helpful.md",
            ".claude/agents/reviewer.md",
            "src/main.rs",
            ".gitignore",
            ".github/ci.yml",
        ] {
            check_write(&policy, &dir.path().join(allowed))
                .unwrap_or_else(|e| panic!("{allowed} must be writable: {e}"));
        }

        // Outside the root is still refused — removing the metadata rule did not
        // remove the boundary.
        let outside = dir.path().parent().unwrap().join("hrdr-outside-probe");
        check_write(&policy, &outside).unwrap_err();

        // …including through a symlink, which is why the check is on canonical
        // paths rather than on the string the model typed.
        #[cfg(unix)]
        {
            let link = dir.path().join("escape");
            std::os::unix::fs::symlink(dir.path().parent().unwrap(), &link).unwrap();
            check_write(&policy, &link.join("hrdr-outside-probe")).unwrap_err();
        }
    }

    /// hrdr can itself be launched inside a linked worktree (the user made one,
    /// or another harness did), where `<cwd>/.git` is a *file* pointing at the
    /// parent repo and a commit writes objects and refs that live outside the
    /// worktree entirely. [`git_metadata_roots`] is what keeps that working.
    #[test]
    fn hrdr_inside_a_linked_worktree_still_commits() {
        if which::which("git").is_err() {
            return; // best-effort: exercise the real backend when available
        }
        let dir = tempfile::tempdir().unwrap();
        let (_repo, wt) = repo_with_linked_worktree(dir.path());
        // The sub-agent shape from `for_agent`, minus the temp/scratch roots
        // that would cover the whole test tree.
        let mut roots = vec![wt.clone()];
        roots.extend(git_metadata_roots(&wt));
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: canonical_roots(roots),
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };

        check_write(&policy, &wt.join("f.txt")).unwrap();

        // hrdr's plumbing, spelled the way `task_*` spells it.
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&wt)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        git(&["add", "f.txt"]);
        git(&[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "mine",
        ]);
        assert!(
            git(&["log", "--oneline"]).contains("mine"),
            "the commit did not land"
        );
    }

    #[test]
    fn seatbelt_profile_lists_every_writable_root() {
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![PathBuf::from("/work/wt"), PathBuf::from("/tmp/scratch")],
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };
        assert_eq!(
            seatbelt_profile(SandboxMode::Write, &policy),
            concat!(
                "(version 1)\n",
                "(deny default)\n",
                "(allow process-fork)\n",
                "(allow process-exec*)\n",
                "(allow signal)\n",
                "(allow sysctl-read)\n",
                "(allow mach-lookup)\n",
                "(allow ipc-posix*)\n",
                "(allow file-read*)\n",
                "(allow file-write* (subpath \"/dev/null\") (subpath \"/dev/zero\") (subpath \"/dev/random\") (subpath \"/dev/urandom\"))\n",
                "(allow file-write* (subpath \"/work/wt\") (subpath \"/tmp/scratch\"))\n",
                "(allow network*)\n",
            )
        );

        // A `/private/…` root also grants its public spelling — defensive, not
        // the fix for the watch tool's macOS test (that was the `/dev/null`
        // write above; the check died on `2>/dev/null` before the roots line
        // was ever reached). Roots without the prefix gain nothing (the
        // whole-profile assertion above pins that), and a root that is already
        // the public spelling is not duplicated.
        let mac = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![
                PathBuf::from("/private/var/folders/ab/T"),
                PathBuf::from("/Users/me/work"),
            ],
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };
        let profile = seatbelt_profile(SandboxMode::Write, &mac);
        assert!(
            profile.contains(
                "(allow file-write* (subpath \"/private/var/folders/ab/T\") \
                 (subpath \"/Users/me/work\") (subpath \"/var/folders/ab/T\"))"
            ),
            "{profile}"
        );
        let literal = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![PathBuf::from("/var/folders/ab/T")],
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };
        assert_eq!(
            seatbelt_profile(SandboxMode::Write, &literal)
                .matches("(subpath \"/var/folders/ab/T\")")
                .count(),
            1,
            "a root already in its public spelling is granted once, not re-derived"
        );

        // A quote in a path is escaped, not left to break the profile.
        let odd = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![PathBuf::from("/work/we\"ird")],
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };
        assert!(
            seatbelt_profile(SandboxMode::Write, &odd)
                .contains("(allow file-write* (subpath \"/work/we\\\"ird\"))"),
            "{}",
            seatbelt_profile(SandboxMode::Write, &odd)
        );

        // With no writable roots the write line is absent entirely: an
        // unfiltered `(allow file-write*)` would allow every write there is.
        let empty = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: Vec::new(),
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };
        assert_eq!(
            seatbelt_profile(SandboxMode::Write, &empty)
                .matches("(allow file-write*")
                .count(),
            1,
            "an empty root set must not open project writes — only the device line stays"
        );
    }

    /// The argv as strings, for readable assertions.
    fn argv(args: &[std::ffi::OsString]) -> Vec<String> {
        args.iter().map(|a| a.to_string_lossy().into()).collect()
    }

    /// Every profile says `(allow network*)`, in every mode — the sandbox confines
    /// the filesystem and nothing else.
    ///
    /// Asserted as the WHOLE profile rather than as a substring, because what has
    /// to be true is that nothing else moved: SBPL is last-match-wins, so a stray
    /// later `deny` would undo an earlier `allow` silently and a `contains` check
    /// would never see it.
    #[test]
    fn every_seatbelt_profile_allows_the_network() {
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![PathBuf::from("/work/wt")],
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };
        assert_eq!(
            seatbelt_profile(SandboxMode::Write, &policy),
            concat!(
                "(version 1)\n",
                "(deny default)\n",
                "(allow process-fork)\n",
                "(allow process-exec*)\n",
                "(allow signal)\n",
                "(allow sysctl-read)\n",
                "(allow mach-lookup)\n",
                "(allow ipc-posix*)\n",
                "(allow file-read*)\n",
                "(allow file-write* (subpath \"/dev/null\") (subpath \"/dev/zero\") (subpath \"/dev/random\") (subpath \"/dev/urandom\"))\n",
                "(allow file-write* (subpath \"/work/wt\"))\n",
                "(allow network*)\n",
            )
        );
        for mode in [SandboxMode::Read, SandboxMode::Jail] {
            assert!(
                seatbelt_profile(mode, &policy).contains("(allow network*)"),
                "{mode} must not confine the network"
            );
        }
    }

    /// Read mode grants no writes at all and narrows reads to the system
    /// directories plus the readable roots.
    #[test]
    fn seatbelt_strict_profile_allows_no_writes_and_only_the_read_roots() {
        let policy = SandboxPolicy {
            mode: SandboxMode::Jail,
            writable_roots: Vec::new(),
            readable_roots: vec![PathBuf::from("/work/wt")],
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };
        assert_eq!(
            seatbelt_profile(SandboxMode::Jail, &policy),
            concat!(
                "(version 1)\n",
                "(deny default)\n",
                "(allow process-fork)\n",
                "(allow process-exec*)\n",
                "(allow signal)\n",
                "(allow sysctl-read)\n",
                "(allow mach-lookup)\n",
                "(allow ipc-posix*)\n",
                "(allow file-read* (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\")\n",
                "  (subpath \"/System\") (subpath \"/Library\") (subpath \"/private/etc\")\n",
                "  (subpath \"/dev\") (subpath \"/work/wt\"))\n",
                "(allow network*)\n",
            )
        );
    }

    /// The Seatbelt argv: `-p <profile> -- <shell> -c <cmd>`, and the wrapper
    /// is the pinned absolute path, never a `PATH` lookup.
    #[test]
    fn seatbelt_args_pass_the_profile_then_the_shell_invocation() {
        assert_eq!(SEATBELT_PROGRAM, "/usr/bin/sandbox-exec");
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![PathBuf::from("/work/wt")],
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };
        let args = argv(&seatbelt_args(&policy, crate::Shell::Bash, "echo hi"));
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], seatbelt_profile(SandboxMode::Write, &policy));
        assert_eq!(args[2..], ["--", "bash", "-c", "echo hi"]);
    }

    /// Whether to skip a backend test for want of the backend — **never in CI**.
    ///
    /// Locally a missing `sandbox-exec` or shell is an environment fact and
    /// failing on it says nothing about the code. On a runner it is a broken
    /// environment, and the only useful thing a test can do is fail: a skip that
    /// cannot tell those apart turns an infrastructure failure into a green tick,
    /// which is worse than either. Same reasoning, and same shape, as
    /// `skip_for_want_of_a_pty` in `apps/hrdr/tests/tui_pty.rs`.
    ///
    /// This is why the backlog could still say Seatbelt "has never run": the test
    /// below returned early on both conditions, so a run that exercised nothing
    /// was indistinguishable from one that passed.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn skip_for_want_of(what: &str, present: bool) -> bool {
        if present {
            return false;
        }
        assert!(
            std::env::var_os("CI").is_none(),
            "{what} is missing on a CI runner — that is a broken environment, not a \
             reason to report this backend as tested"
        );
        eprintln!("skipping: {what} is not available on this machine");
        true
    }

    /// CI must exercise a REAL backend, not the `None` fallback. Without this,
    /// every backend test on the runner could be silently confining nothing and
    /// still pass: `detect_backend` degrades to `None` by design, which is right
    /// for a user's machine and wrong for the job that is supposed to prove the
    /// backend works.
    ///
    /// macOS always ships `/usr/bin/sandbox-exec`, so `Seatbelt` is not a guess.
    /// Linux is deliberately not asserted here — Landlock needs the LSM enabled
    /// in `/sys/kernel/security/lsm`, which is a property of the runner image
    /// rather than of this code.
    #[test]
    fn ci_runs_a_real_os_backend() {
        if std::env::var_os("CI").is_none() {
            return;
        }
        if cfg!(windows) {
            assert_eq!(
                detect_backend(),
                OsSandboxBackend::LowIntegrity,
                "every supported Windows has Mandatory Integrity Control, so a CI \
                 run that detects no backend means `current_exe` failed and the \
                 Low-integrity assertions below it are vacuous"
            );
        }
        if cfg!(target_os = "macos") {
            assert_eq!(
                detect_backend(),
                OsSandboxBackend::Seatbelt,
                "macOS ships /usr/bin/sandbox-exec, so a CI run that detects no \
                 backend is a broken environment and every Seatbelt assertion \
                 below it is vacuous"
            );
        }
    }

    /// The wrapper argv, checked on every platform so the shape cannot rot
    /// where it is not built: `__sandbox-exec -- <shell> -c <cmd>`, mirroring
    /// Seatbelt's `-p <profile> -- <shell> -c <cmd>`.
    #[test]
    fn low_integrity_args_wrap_the_shell_invocation() {
        let args = argv(&low_integrity_args(crate::Shell::Bash, "echo hi"));
        assert_eq!(args[0], SANDBOX_EXEC_ARG);
        assert_eq!(args[1], "--");
        assert_eq!(args[2..], ["bash", "-c", "echo hi"]);
    }

    /// A Low-integrity child can write nowhere the user owns, so the backend is
    /// only wired up for `Read` — the mode whose whole definition that is.
    /// `Write` must keep falling through to the notice until its roots can be
    /// labelled, or it would refuse every write it exists to permit.
    #[cfg(windows)]
    #[test]
    fn windows_write_mode_is_not_confined_yet_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![canonicalize_nearest(dir.path())],
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };
        let n = notices();
        let _ = shell_command_with_backend(
            OsSandboxBackend::LowIntegrity,
            crate::Shell::Bash,
            "echo hi",
            &policy,
            &n,
        );
        assert_eq!(n.take().as_deref(), Some(NO_OS_SANDBOX_NOTICE));
    }

    // The end-to-end Windows test does NOT live here. The backend re-execs
    // `std::env::current_exe()`, which in a `hrdr-tools` unit test is the TEST
    // BINARY — this crate ships no hrdr binary to re-exec — so the spawn fed
    // `__sandbox-exec -- …` to libtest as filter arguments and wedged the
    // Windows job for 37 minutes. It is an integration test in
    // `apps/hrdr/tests/sandbox_windows.rs`, where `CARGO_BIN_EXE_hrdr` names the
    // real wrapper.

    /// The real thing, on the only platform that has it: a write outside the
    /// roots is refused by the kernel, a write inside lands.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn shell_write_outside_roots_is_denied_under_seatbelt() {
        if skip_for_want_of(SEATBELT_PROGRAM, Path::new(SEATBELT_PROGRAM).exists()) {
            return;
        }
        let detected = crate::Shell::detect();
        if skip_for_want_of("a shell (bash or sh)", detected.is_some()) {
            return;
        }
        let shell = detected.expect("checked above");
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // Struct literal, not `for_agent`: a writable `env::temp_dir()` root
        // would cover the "outside" tempdir too (the slice-3/5/6 trap).
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![canonicalize_nearest(dir.path())],
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };

        let target = canonicalize_nearest(outside.path()).join("escaped");
        let mut cmd = shell_command_with_backend(
            OsSandboxBackend::Seatbelt,
            shell,
            &format!("echo x > {}", target.display()),
            &policy,
            &notices(),
        );
        cmd.current_dir(dir.path());
        let out = cmd.output().await.unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(!out.status.success(), "the write was allowed: {stderr}");
        assert!(stderr.contains("Operation not permitted"), "{stderr}");
        assert!(!target.exists(), "the write landed anyway");

        let inside = canonicalize_nearest(dir.path()).join("mine");
        let mut cmd = shell_command_with_backend(
            OsSandboxBackend::Seatbelt,
            shell,
            &format!("echo x > {}", inside.display()),
            &policy,
            &notices(),
        );
        cmd.current_dir(dir.path());
        let out = cmd.output().await.unwrap();
        assert!(
            out.status.success(),
            "the cwd write was blocked: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "x");
    }

    /// The canonical skip guard for the end-to-end tests: this machine's real
    /// backend when it has one, plus a shell to run through it.
    #[cfg(target_os = "linux")]
    fn confined_shell() -> Option<crate::Shell> {
        if detect_backend() == OsSandboxBackend::None {
            return None; // best-effort: exercise the real backend when available
        }
        crate::Shell::detect()
    }

    /// Run `command` through the real `shell` tool with `ctx`'s policy.
    #[cfg(target_os = "linux")]
    async fn run_shell(shell: crate::Shell, ctx: &crate::ToolContext, command: &str) -> String {
        use crate::Tool as _;
        crate::ShellTool::new(shell)
            .execute(
                serde_json::json!({"command": command, "timeout_secs": 60}),
                ctx,
            )
            .await
            .unwrap()
    }

    /// A write outside the roots dies in the kernel, not in the guard: the
    /// mount is read-only, so the redirect fails and nothing is created. This
    /// is the escape that motivated the whole feature, at the shell layer.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_write_outside_roots_is_refused_by_the_kernel() {
        let Some(shell) = confined_shell() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // A `for_agent` policy would bind `env::temp_dir()` writable and the
        // second tempdir with it; confine to the cwd alone.
        let ctx = confined_ctx(dir.path(), SandboxMode::Write);

        // The wording is the backend's, not ours: both EACCES (Landlock) and
        // EROFS (a read-only filesystem) are asserted because the *property* is
        // what matters — the write does not land.
        let refused =
            |out: &str| out.contains("Read-only file system") || out.contains("Permission denied");

        let target = outside.path().join("escaped");
        let out = run_shell(shell, &ctx, &format!("echo x > {}", target.display())).await;
        assert!(refused(&out), "{out}");
        assert!(!target.exists(), "the write landed anyway");

        // …including the shape actually observed in the wild: `cd` out first.
        let out = run_shell(
            shell,
            &ctx,
            &format!("cd {} && echo x > escaped2", outside.path().display()),
        )
        .await;
        assert!(refused(&out), "{out}");
        assert!(!outside.path().join("escaped2").exists());
    }

    /// The flip side: everything the default root set covers really is
    /// writable inside the sandbox, or no build would survive it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_write_in_cwd_and_tmp_succeeds_confined() {
        let Some(shell) = confined_shell() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = crate::ToolContext::new(dir.path().to_path_buf());
        ctx.sandbox = std::sync::Arc::new(SandboxPolicy::for_agent(
            SandboxMode::Write,
            dir.path(),
            &[],
        ));

        let in_cwd = dir.path().join("in-cwd");
        let in_scratch = session_scratch_dir().join("confined-write-probe");
        let out = run_shell(
            shell,
            &ctx,
            &format!(
                "echo a > {} && echo b > {}",
                in_cwd.display(),
                in_scratch.display()
            ),
        )
        .await;
        assert!(!out.contains("[exit status"), "{out}");
        assert_eq!(std::fs::read_to_string(&in_cwd).unwrap().trim(), "a");
        assert_eq!(std::fs::read_to_string(&in_scratch).unwrap().trim(), "b");
        let _ = std::fs::remove_file(&in_scratch);
    }

    /// hrdr launched inside a linked worktree commits THERE and nowhere else:
    /// [`git_metadata_roots`] grants the shared object store and the `hrdr/` ref
    /// namespace, never the parent `.git` itself, so a commit against the parent
    /// checkout is still refused at the OS layer.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_linked_worktree_commits_but_the_parent_repo_stays_blocked() {
        let Some(shell) = confined_shell() else {
            return;
        };
        if which::which("git").is_err() {
            return; // best-effort: exercise the real backend when available
        }
        let dir = tempfile::tempdir().unwrap();
        let (repo, wt) = repo_with_linked_worktree(dir.path());

        // Struct literal, not `for_agent`: the repo lives in a tempdir, and a
        // writable `env::temp_dir()` root would make the parent writable too
        // and void the whole test.
        let mut roots = vec![canonicalize_nearest(&wt)];
        roots.extend(
            git_metadata_roots(&wt)
                .iter()
                .map(|r| canonicalize_nearest(r)),
        );
        let mut ctx = crate::ToolContext::new(wt.clone());
        ctx.sandbox = std::sync::Arc::new(SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: roots,
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        });

        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        let ident = "-c user.email=t@example.com -c user.name=t";
        let out = run_shell(
            shell,
            &ctx,
            &format!("git add f.txt && git {ident} commit -q -m mine"),
        )
        .await;
        assert!(
            !out.contains("[exit status"),
            "the worktree commit failed: {out}"
        );
        let log = std::process::Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&wt)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&log.stdout).contains("mine"),
            "the commit did not land: {log:?}"
        );

        // The parent repo's index is outside the roots, so committing there
        // dies before it can touch a ref.
        let out = run_shell(
            shell,
            &ctx,
            &format!(
                "git -C {} {ident} commit --allow-empty -m escaped",
                repo.display()
            ),
        )
        .await;
        assert!(
            out.contains("[exit status"),
            "the parent commit succeeded: {out}"
        );
        let log = std::process::Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&log.stdout).contains("escaped"),
            "a commit landed on the parent repo: {log:?}"
        );
    }

    /// **`jail`'s read confinement is in-process, and that is the whole of it.**
    ///
    /// Read confinement in `jail` mode is entirely in-process via
    /// [`SandboxPolicy::check_read`], applied on every tool call. There is no OS
    /// mount to confine — and a jailed agent has no `shell`, no `verify`, and no
    /// LSP, so nothing it calls ever spawns a subprocess for an OS layer to
    /// confine anyway. The in-process check works on every platform with no
    /// backend at all.
    #[test]
    fn jails_read_confinement_is_the_in_process_guard() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Jail, dir.path(), &[]);

        let inside = dir.path().join("audit-me.rs");
        assert!(
            policy
                .check_read(&canonicalize_nearest(&inside), &inside)
                .is_ok(),
            "its own working directory is readable, or it cannot audit anything"
        );
        for outside in [Path::new("/etc/hostname"), Path::new("/usr/bin/env")] {
            let err = policy
                .check_read(&canonicalize_nearest(outside), outside)
                .expect_err("outside the roots must be refused")
                .to_string();
            assert!(err.contains("strictly confined"), "{err}");
        }
        // And nothing is writable — with no execution there is nothing that needs a
        // writable /tmp.
        assert!(policy.writable_roots.is_empty());
    }

    /// The timeout still reaps the whole tree with a backend in the way.
    ///
    /// Landlock adds no process wrapper — the shell is the direct child, confining
    /// itself between fork and exec — so the existing group-kill reaches every
    /// descendant exactly as it does unsandboxed. Kept as a marker-file test rather
    /// than a pid probe because it also covers Seatbelt, which does interpose a
    /// wrapper process: the marker only appears if the grandchild outlived the kill.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timeout_kill_reaches_through_the_sandbox() {
        let Some(shell) = confined_shell() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = confined_ctx(dir.path(), SandboxMode::Write);
        ctx.enforce_timeout_floor = false;
        let marker = dir.path().join("grandchild-finished");

        let command = format!("(sleep 5 && touch {m}) & sleep 5", m = marker.display());
        use crate::Tool as _;
        let out = crate::ShellTool::new(shell)
            .execute(
                serde_json::json!({"command": command, "timeout_secs": 1}),
                &ctx,
            )
            .await
            .expect_err("a killed command is not a successful one")
            .to_string();
        assert!(out.contains("timed out"), "{out}");

        // Well past the grandchild's own sleep: if it were alive it would
        // have touched the marker by now.
        tokio::time::sleep(std::time::Duration::from_millis(5500)).await;
        assert!(
            !marker.exists(),
            "the backgrounded grandchild survived the group kill"
        );
    }

    /// **The ssh / user-namespace failure class is gone, and a bare
    /// "bad owner or permissions" must not be blamed on the sandbox any more.**
    ///
    /// The old sandbox used an unprivileged user namespace that remapped the
    /// invoking uid — every root-owned file read as `nobody`, and OpenSSH refused
    /// any config file it couldn't vouch for, killing every `git push` over ssh.
    /// No namespace means no misread ownership.
    ///
    /// So the denial note for it is deleted too. On a machine with no namespace
    /// involved, that message means the user's own `~/.ssh/config` really is
    /// group-writable — a true local problem hrdr must report as-is rather than
    /// editorialize over.
    #[test]
    fn an_ssh_ownership_complaint_is_no_longer_blamed_on_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let write = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        assert_eq!(
            sandbox_denial_note(&write, "Bad owner or permissions on /etc/ssh/ssh_config"),
            None,
            "no user namespace, no sandbox explanation to offer"
        );
    }

    /// A confined agent can commit its own work, proved against the real OS
    /// backend rather than against the argv.
    ///
    /// This is the reversal the redesign turns on: `.git` used to be subtracted
    /// from a write sub-agent's mounts, so `git add`/`commit`/`update-ref` all
    /// died on EROFS. An agent working in the user's project is now assumed to
    /// have authority over that project, and a sub-agent told to commit its own
    /// changes can. Asserted as a *property* of whatever backend this machine
    /// runs, so re-introducing the lock on any of them fails here.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_confined_agent_can_commit_its_own_work() {
        let Some(shell) = confined_shell() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let repo = canonicalize_nearest(dir.path());
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .expect("git")
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "before\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-qm", "init"]);
        if !repo.join(".git").is_dir() {
            return; // git unavailable — nothing to prove
        }

        let mut ctx = crate::ToolContext::new(repo.clone());
        ctx.sandbox = std::sync::Arc::new(SandboxPolicy::for_agent(SandboxMode::Write, &repo, &[]));
        let run = |command: String| {
            let ctx = ctx.clone();
            async move {
                use crate::Tool as _;
                crate::ShellTool::new(shell)
                    .execute(serde_json::json!({"command": command}), &ctx)
                    .await
                    .map_err(|e| e.to_string())
                    .unwrap_or_else(|e| e)
            }
        };

        let log = run("git log --oneline".to_string()).await;
        assert!(log.contains("init"), "history stays readable: {log}");

        let edit = run("printf after > f.txt".to_string()).await;
        assert!(!edit.to_lowercase().contains("read-only"), "{edit}");

        // The line that matters. Staging writes the index; committing writes an
        // object and moves a ref. Both live in `.git`, and both must work.
        let commit = run("git add f.txt && git commit -qm mine".to_string()).await;
        assert!(
            !commit.to_lowercase().contains("read-only"),
            "a confined agent must be able to commit: {commit}"
        );
        let head = git(&["log", "--oneline"]);
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).lines().count(),
            2,
            "the commit landed"
        );

        // A ref write directly, the other way in.
        let ref_write = run("git update-ref refs/heads/scratch HEAD".to_string()).await;
        assert!(
            !ref_write.to_lowercase().contains("read-only"),
            "ref writes work too: {ref_write}"
        );
    }

    /// **A confined shell still reaches the network, in every mode.** Proved
    /// against the real backend — the old sandbox used a private network namespace,
    /// so a `/dev/tcp` connect to a listener on the host's own loopback found
    /// nothing. No namespace means a bare connect reaches what is actually there.
    ///
    /// The listener is one this test bound itself, so nothing external is needed
    /// and a CI runner with no egress cannot fail it for the wrong reason. Never
    /// accepted, deliberately: the kernel completes the handshake out of the listen
    /// backlog, so there is no server thread to start or join.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_confined_shell_still_reaches_the_network() {
        let Some(shell) = confined_shell() else {
            return;
        };
        if shell != crate::Shell::Bash {
            return; // the probe is bash's `/dev/tcp`; POSIX sh has no equivalent
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let port = listener.local_addr().unwrap().port();
        let probe = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo CONNECTED");
        let host = std::process::Command::new("bash")
            .args(["-c", &probe])
            .output()
            .expect("bash");
        if !host.status.success() {
            return; // a bash without `--enable-net-redirections` proves nothing
        }

        let dir = tempfile::tempdir().unwrap();
        let mine = notices();
        for mode in [SandboxMode::Write, SandboxMode::Read] {
            let policy = SandboxPolicy::for_agent(mode, dir.path(), &[]);
            let mut cmd = sandboxed_shell_command(shell, &probe, &policy, &mine);
            cmd.current_dir(dir.path());
            let out = cmd.output().await.unwrap();
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("CONNECTED"),
                "{mode} must not confine the network: {out:?}"
            );
        }
    }

    /// Every notice assertion below owns its own channel, so none of them can
    /// interleave with another — which is what the process-global cell used to
    /// need a test-only mutex for.
    fn notices() -> SandboxNotices {
        SandboxNotices::default()
    }

    /// Landlock really does block a write outside the roots.
    ///
    /// The backend is forced rather than detected, so this arm runs on a kernel
    /// without the LSM too. The policy is a struct literal because a `for_agent`
    /// policy makes `env::temp_dir()` writable and the "outside" tempdir with it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn landlock_blocks_writes_outside_roots() {
        if !std::fs::read_to_string("/sys/kernel/security/lsm")
            .unwrap_or_default()
            .contains("landlock")
        {
            return; // best-effort: exercise the real backend when available
        }
        let Some(shell) = crate::Shell::detect() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![canonicalize_nearest(dir.path())],
            readable_roots: Vec::new(),
            cache_roots: Vec::new(),
            wrap_tool_results: false,
        };

        let target = outside.path().join("escaped");
        let mut cmd = shell_command_with_backend(
            OsSandboxBackend::Landlock,
            shell,
            &format!("echo x > {}", target.display()),
            &policy,
            &notices(),
        );
        cmd.current_dir(dir.path());
        let out = cmd.output().await.unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(!out.status.success(), "the write was allowed: {stderr}");
        assert!(stderr.contains("Permission denied"), "{stderr}");
        assert!(!target.exists(), "the write landed anyway");

        // …and the cwd stays writable, or no agent could work at all.
        let inside = dir.path().join("mine");
        let mut cmd = shell_command_with_backend(
            OsSandboxBackend::Landlock,
            shell,
            &format!("echo x > {}", inside.display()),
            &policy,
            &notices(),
        );
        cmd.current_dir(dir.path());
        let out = cmd.output().await.unwrap();
        assert!(
            out.status.success(),
            "the cwd write was blocked: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "x");
    }

    /// With no backend the command runs unconfined — allowed, but only ever
    /// once the user has been told, and only told once *to that agent*.
    #[test]
    fn no_backend_emits_the_not_confined_notice_once() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        let mine = notices();

        let _cmd = shell_command_with_backend(
            OsSandboxBackend::None,
            crate::Shell::Bash,
            "true",
            &policy,
            &mine,
        );
        assert_eq!(
            mine.take().as_deref(),
            Some(NO_OS_SANDBOX_NOTICE),
            "the first unconfined command must say so"
        );

        let _cmd = shell_command_with_backend(
            OsSandboxBackend::None,
            crate::Shell::Bash,
            "true",
            &policy,
            &mine,
        );
        assert_eq!(
            mine.take(),
            None,
            "the same notice must not repeat every command"
        );

        // The recurrence is what gets silenced, never the sibling: a second
        // agent running unconfined is told, whatever the first was told.
        let sibling = notices();
        let _cmd = shell_command_with_backend(
            OsSandboxBackend::None,
            crate::Shell::Bash,
            "true",
            &policy,
            &sibling,
        );
        assert_eq!(sibling.take().as_deref(), Some(NO_OS_SANDBOX_NOTICE));
    }

    #[test]
    fn sandbox_notice_is_take_once_per_agent() {
        let msg = "sandbox: test notice — take once".to_string();
        let mine = notices();
        mine.set(msg.clone());
        assert_eq!(mine.take().as_deref(), Some(msg.as_str()));
        assert_eq!(mine.take(), None);
        // The same message never notices twice to the same agent…
        mine.set(msg.clone());
        assert_eq!(mine.take(), None);
        // …and a sibling's queue knows nothing about any of that.
        let sibling = notices();
        sibling.set(msg.clone());
        assert_eq!(sibling.take().as_deref(), Some(msg.as_str()));
    }

    /// The one remaining notice is pinned bytes: it is what the user reads when
    /// there is no OS confinement at all, and it must not soften into something
    /// that could be mistaken for "confined".
    ///
    /// One, now, because every other degradation notice described a fallback that
    /// no longer exists — Landlock absent, jail's read mounts gone. A backend is
    /// available or it is not.
    #[test]
    fn the_unconfined_notice_says_exactly_what_is_not_confined() {
        assert_eq!(
            NO_OS_SANDBOX_NOTICE,
            "sandbox: no OS-level sandbox is available on this system — shell commands are NOT \
             OS-confined; the file tools remain guarded. Use --sandbox none to silence this."
        );
    }
}

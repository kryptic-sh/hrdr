//! The verification gate: what *this* project counts as checked, discovered
//! instead of assumed.
//!
//! [`VerificationLedger`](crate::VerificationLedger) can say a run was partial,
//! but on its own it has no idea what a *complete* run would have been — it
//! falls back to a built-in notion of "whole tree" that is right for some
//! projects and wrong for others. This module supplies the missing half: the
//! concrete commands the project itself treats as its gate.
//!
//! Two sources, in order:
//!
//! 1. **The project's CI.** Whatever a maintainer wired into CI *is* the
//!    definition — it is the thing that turns a pull request red. Rather than a
//!    parser per provider, this parses the config as YAML and walks it looking
//!    for the handful of keys that hold shell (`run`, `script`, `commands`,
//!    `command`, `before_script`). GitHub Actions, GitLab CI, CircleCI, Azure
//!    Pipelines, Bitbucket, Drone/Woodpecker, Travis, Cirrus and Buildkite all
//!    fall out of that one walk, because their difference is *where* in the
//!    document the commands sit — and a recursive walk does not care. Jenkins is
//!    the exception (Groovy, not YAML) and gets a small scanner of its own.
//! 2. **Ecosystem convention.** No CI, or CI with nothing recognisable in it,
//!    falls back to what the ecosystem's own tooling would run: `cargo` for a
//!    Cargo.toml, the declared scripts for a package.json, `pytest`/`ruff` for a
//!    pyproject, `go test ./...` for a go.mod, and so on down a table of the
//!    common ones. A guess, and labelled as one in the prompt.
//!
//! Command strings are only ever *read* here. Nothing in this module runs
//! anything, and the gate is advisory in exactly the way the ledger's note is:
//! it tells the model what the bar is, so that clearing a lower one is a visible
//! choice rather than an accident.

use std::path::{Path, PathBuf};

use crate::verification::{
    CheckKind, Scope, classify, classify_segment, normalized_tokens, segments,
};

/// One command the project gates on, and the question it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateCheck {
    pub kind: CheckKind,
    pub command: String,
}

/// Where a gate came from — which is what decides how firmly the prompt states
/// it. A CI-derived gate is a fact about the project; an ecosystem-derived one
/// is a convention we are applying on the project's behalf, and saying so is
/// what lets the model correct it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateSource {
    Ci,
    Ecosystem,
}

/// The project's verification gate. `checks` is empty when nothing could be
/// discovered — a data directory, a language we have no table for — and every
/// consumer treats an empty gate as "no gate", never as "nothing to run".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gate {
    pub checks: Vec<GateCheck>,
    /// `None` exactly when `checks` is empty.
    pub source: Option<GateSource>,
    /// What the gate was read out of, relative to the project root — the CI
    /// files that contributed, or the manifests that named the ecosystems.
    /// Named in the prompt so a wrong gate is traceable to a file rather than to
    /// hrdr.
    pub origins: Vec<String>,
}

/// The order checks are listed and run in: cheap and broad first, slow and
/// narrow last, which is the order a person would run them by hand. Distinct
/// from [`CheckKind`]'s own ordering (a reading order) and from the ledger's
/// `kind_rank` (how much a kind proves) — three different questions that only
/// look alike.
fn gate_rank(kind: CheckKind) -> u8 {
    match kind {
        CheckKind::Format => 0,
        CheckKind::Lint => 1,
        CheckKind::TypeCheck => 2,
        CheckKind::Build => 3,
        CheckKind::Test => 4,
    }
}

impl Gate {
    /// Discover the gate for the project rooted at `root`: CI if it can be read,
    /// ecosystem convention otherwise.
    ///
    /// Every filesystem error is a miss, not a failure. A gate is an
    /// improvement over having none, and a session must not fail to start
    /// because a workflow file is unreadable.
    pub fn detect(root: &Path) -> Self {
        let (checks, origins) = from_ci(root);
        if !checks.is_empty() {
            return Self::build(checks, GateSource::Ci, origins);
        }
        let (checks, origins) = from_ecosystems(root);
        if !checks.is_empty() {
            return Self::build(checks, GateSource::Ecosystem, origins);
        }
        Self::default()
    }

    /// One check per kind *per tool*, in run order.
    ///
    /// A real CI config names several commands per kind — three test jobs across
    /// two workflows — and listing them all would turn a four-line prompt
    /// section into a forty-line one the model skims. The gate is the *minimum
    /// bar*, so among commands that answer the same question with the same tool
    /// it keeps the broadest: a project-wide run beats a narrowed one, and among
    /// equals the first in file order wins, which keeps the answer stable across
    /// runs.
    ///
    /// Per *tool*, not per kind, because a polyglot repo's `cargo test` and
    /// `npm run test` are not two spellings of one check — they cover different
    /// halves of the tree, and keeping only one would excuse the other.
    fn build(checks: Vec<GateCheck>, source: GateSource, origins: Vec<String>) -> Self {
        let mut best: Vec<GateCheck> = Vec::new();
        for check in checks {
            let family = tool_family(&check.command);
            match best
                .iter_mut()
                .find(|b| b.kind == check.kind && tool_family(&b.command) == family)
            {
                Some(existing) => {
                    if !is_whole(&existing.command) && is_whole(&check.command) {
                        existing.command = check.command;
                    }
                }
                None => best.push(check),
            }
        }
        best.sort_by_key(|c| gate_rank(c.kind));
        best.truncate(MAX_CHECKS);
        Self {
            checks: best,
            source: Some(source),
            origins,
        }
    }

    /// Whether anything was discovered.
    pub fn is_empty(&self) -> bool {
        self.checks.is_empty()
    }

    /// The kinds this project gates on — what the ledger owes a passing run of.
    pub fn kinds(&self) -> Vec<CheckKind> {
        self.checks.iter().map(|c| c.kind).collect()
    }

    /// Whether `command` runs one of the gate's checks, and what that proves.
    ///
    /// The match is by token *subset*: a run counts when it contains every token
    /// of a gate command, so adding `--all-features` or `--locked` to the gate's
    /// own invocation still counts, while dropping `--workspace` from it does
    /// not. Exact-string matching would be defeated by whitespace and by the
    /// first extra flag; the ledger's own classifier is the fallback either way.
    ///
    /// The scope answer is where this earns its place. This repo's CI runs
    /// `cargo clippy --all-targets -- -D warnings`, which the classifier calls
    /// *partial* — correctly, in general, since it names no workspace. But it is
    /// what CI runs, so a session that ran it has cleared the project's lint bar
    /// and should not be nagged about it. So: a run matching the gate is whole
    /// unless the gate command itself is whole and the run narrowed it (`pytest`
    /// as the gate, `pytest -k slow` as the run).
    pub fn matched(&self, command: &str) -> Option<(CheckKind, Scope)> {
        let run: Vec<Vec<String>> = segments(command).map(|s| normalized_tokens(&s)).collect();
        let check = self.checks.iter().find(|check| {
            let want = normalized_tokens(&check.command);
            !want.is_empty() && run.iter().any(|got| want.iter().all(|t| got.contains(t)))
        })?;
        let run_whole = is_whole(command);
        let scope = if run_whole || !is_whole(&check.command) {
            Scope::Whole
        } else {
            Scope::Partial
        };
        Some((check.kind, scope))
    }

    /// The gate as a one-command-per-line list, for a prompt or a note.
    pub fn command_list(&self) -> String {
        self.checks
            .iter()
            .map(|c| format!("- `{}` ({})", c.command, c.kind.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The gate commands answering `kinds`, inline — what a one-line note tells
    /// the model to run. Empty when the gate covers none of them, which is the
    /// case for a kind the session ran off its own bat.
    pub fn owed_commands(&self, kinds: &[CheckKind]) -> String {
        self.checks
            .iter()
            .filter(|c| kinds.contains(&c.kind))
            .map(|c| format!("`{}`", c.command))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Where it came from, as a phrase to drop into a sentence.
    pub fn origin_phrase(&self) -> String {
        match self.source {
            Some(GateSource::Ci) => format!("read from {}", self.origins.join(", ")),
            Some(GateSource::Ecosystem) => format!(
                "no CI configuration found, so these are the standard checks for {}",
                self.origins.join(" + ")
            ),
            None => String::new(),
        }
    }
}

/// Whether a command claims the whole project, per the ledger's classifier.
fn is_whole(command: &str) -> bool {
    classify(command).is_some_and(|(_, s)| s == Scope::Whole)
}

/// Hard cap on the listed checks. A gate the model has to scroll is one it stops
/// reading, and a repo whose CI has more than eight distinct check tools has a
/// gate we cannot usefully summarise anyway.
const MAX_CHECKS: usize = 8;

/// The tool a command runs, as the identity two commands are compared on:
/// `cargo test -p x` and `cargo test --workspace` are the same check, `cargo
/// test` and `npm run test` are not.
fn tool_family(command: &str) -> String {
    normalized_tokens(command)
        .first()
        .map(|t| t.rsplit(['/', '\\']).next().unwrap_or(t).to_string())
        .unwrap_or_default()
}

// ── CI configuration ───────────────────────────────────────────────────────

/// Largest CI file read. Workflow files are kilobytes; anything past this is not
/// one, and reading it would be paying to parse a data blob.
const MAX_CI_BYTES: u64 = 512 * 1024;

/// How many *scanned* CI files are read at all. A monorepo can carry dozens of
/// workflows, and the gate is a summary — reading the first handful in sorted
/// order gets the same answer as reading all of them, deterministically. That
/// holds within the workflow directories; the single-file configs are counted
/// separately, since dropping a whole provider is not "the same answer".
const MAX_CI_FILES: usize = 16;

/// Keys whose value is shell, across every provider. `command` is CircleCI's
/// `run: {name, command}` form; `commands` is Drone/Woodpecker; `script` covers
/// GitLab, Travis, Azure and Bitbucket; `run` covers GitHub Actions and
/// CircleCI's short form.
const COMMAND_KEYS: &[&str] = &[
    "run",
    "script",
    "commands",
    "command",
    "before_script",
    "after_script",
    "scripts",
];

/// Whether a mapping key holds shell.
///
/// The suffix arm is Cirrus, whose keys are *named* after what they do —
/// `test_script`, `lint_script`, `build_scripts` — so the key set is open. It is
/// also how GitLab spells `before_script`/`after_script`, which the exact list
/// covers anyway; matching the suffix means a spelling nobody has thought of yet
/// still reads.
fn is_command_key(key: &str) -> bool {
    COMMAND_KEYS.contains(&key) || key.ends_with("_script") || key.ends_with("_scripts")
}

/// The gate as read from CI, plus the files it was read from.
fn from_ci(root: &Path) -> (Vec<GateCheck>, Vec<String>) {
    let mut checks = Vec::new();
    let mut origins = Vec::new();
    for path in ci_files(root) {
        let Some(text) = read_capped(&path) else {
            continue;
        };
        let label = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let commands = if is_jenkinsfile(&path) {
            commands_in_groovy(&text)
        } else {
            commands_in_yaml(&text)
        };
        let found = checks_in(&commands);
        if !found.is_empty() {
            origins.push(label);
            checks.extend(found);
        }
    }
    (checks, origins)
}

/// The CI configuration files present, in a stable order.
fn ci_files(root: &Path) -> Vec<PathBuf> {
    /// Single-file configurations, by their conventional path.
    const NAMED: &[&str] = &[
        ".gitlab-ci.yml",
        ".gitlab-ci.yaml",
        ".circleci/config.yml",
        ".circleci/config.yaml",
        "azure-pipelines.yml",
        "azure-pipelines.yaml",
        "bitbucket-pipelines.yml",
        "bitbucket-pipelines.yaml",
        ".travis.yml",
        ".drone.yml",
        ".woodpecker.yml",
        ".woodpecker.yaml",
        ".cirrus.yml",
        "cloudbuild.yaml",
        "appveyor.yml",
        ".appveyor.yml",
        "Jenkinsfile",
    ];
    /// Directories whose every YAML file is a pipeline.
    const DIRS: &[&str] = &[
        ".github/workflows",
        ".woodpecker",
        ".buildkite",
        ".gitlab/ci",
    ];

    let mut out: Vec<PathBuf> = Vec::new();
    for dir in DIRS {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("yml") | Some("yaml")
                )
            })
            .collect();
        // Sorted, so which files survive `MAX_CI_FILES` does not depend on the
        // order the filesystem happened to hand them back.
        found.sort();
        out.extend(found);
    }
    // Cap the directory scan *before* the named configs go on. `MAX_CI_FILES`
    // is justified by "the first handful in sorted order gets the same answer",
    // which holds inside one workflow directory and not at all across
    // providers: truncating the combined list would let a monorepo with
    // sixteen workflows hide `.gitlab-ci.yml` — a different pipeline, possibly
    // the only one carrying the gate — entirely. `NAMED` is a fixed, short
    // list, so appending it unconditionally still bounds the total.
    out.truncate(MAX_CI_FILES);
    for name in NAMED {
        let path = root.join(name);
        if path.is_file() {
            out.push(path);
        }
    }
    out
}

fn is_jenkinsfile(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("Jenkinsfile"))
}

/// Read a file, refusing anything implausibly large for a config.
fn read_capped(path: &Path) -> Option<String> {
    let len = std::fs::metadata(path).ok()?.len();
    if len > MAX_CI_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Every shell string in a YAML pipeline, wherever it sits in the document.
///
/// The recursion is the point: it is what makes this provider-agnostic. GitHub
/// puts its commands at `jobs.*.steps[].run` and GitLab at `<job>.script`, but
/// neither path is hard-coded — the walk finds a `run:` or a `script:` at any
/// depth, so a provider we have never heard of works too as long as it spells
/// the key one of the handful of ways anyone does.
fn commands_in_yaml(text: &str) -> Vec<String> {
    let Ok(doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_commands(&doc, false, &mut out);
    out
}

/// Walk `value`, collecting strings that are shell. `under_command` says the
/// value was reached through a command key, so a bare string is shell rather
/// than a name; it does not propagate past one level of mapping, because
/// `run: {name: …, command: …}` puts a *label* next to the command.
fn collect_commands(value: &serde_yaml_ng::Value, under_command: bool, out: &mut Vec<String>) {
    use serde_yaml_ng::Value;
    match value {
        Value::String(s) => {
            if under_command {
                out.push(s.clone());
            }
        }
        Value::Sequence(items) => {
            for item in items {
                collect_commands(item, under_command, out);
            }
        }
        Value::Mapping(map) => {
            for (k, v) in map {
                let key = k.as_str().unwrap_or_default().to_ascii_lowercase();
                collect_commands(v, is_command_key(&key), out);
            }
        }
        _ => {}
    }
}

/// Jenkins pipelines are Groovy, so the YAML walk cannot see them. What they do
/// have is a single overwhelmingly conventional spelling — `sh 'cmd'`, `sh
/// "cmd"`, `sh '''…'''` — and pulling the quoted argument out of those lines
/// covers the shape people actually write without pretending to parse Groovy.
fn commands_in_groovy(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("sh ")
            .or_else(|| line.strip_prefix("sh("))
            .or_else(|| line.strip_prefix("bat "))
        else {
            continue;
        };
        let rest = rest.trim().trim_start_matches('(').trim();
        for quote in ["'''", "\"\"\"", "'", "\""] {
            if let Some(body) = rest.strip_prefix(quote)
                && let Some(end) = body.find(quote)
            {
                out.push(body[..end].to_string());
                break;
            }
        }
    }
    out
}

/// Turn raw shell strings into gate checks: split to lines, split each line into
/// the pieces the shell would run separately, and keep the pieces the ledger's
/// classifier recognises. Everything else — `actions/checkout`, `apt-get
/// install`, `set -euo pipefail` — classifies to nothing and drops out, which is
/// why no filtering rules are needed here.
fn checks_in(commands: &[String]) -> Vec<GateCheck> {
    let mut out = Vec::new();
    for block in commands {
        for line in block.lines() {
            let line = line.trim().trim_end_matches('\\').trim();
            // A GitHub expression is a template, not a command: `cargo test
            // ${{ matrix.flags }}` cannot be run as written, and telling the
            // model to run it would be telling it to run something that fails.
            if line.is_empty() || line.contains("${{") {
                continue;
            }
            for segment in segments(line) {
                let Some((kind, _)) = classify_segment(&segment) else {
                    continue;
                };
                let command = normalized_tokens(&segment).join(" ");
                if command.is_empty() {
                    continue;
                }
                out.push(GateCheck { kind, command });
            }
        }
    }
    out
}

// ── ecosystem conventions ──────────────────────────────────────────────────

/// The fallback gate: what each detected ecosystem's own tooling would run.
///
/// Every ecosystem present contributes, so a Rust service with a TypeScript
/// frontend gets both — a polyglot repo whose gate named only half of itself
/// would be a gate that quietly excuses the other half.
fn from_ecosystems(root: &Path) -> (Vec<GateCheck>, Vec<String>) {
    let mut checks = Vec::new();
    let mut origins = Vec::new();
    let mut add = |manifest: &str, found: Vec<GateCheck>| {
        if !found.is_empty() {
            origins.push(manifest.to_string());
            checks.extend(found);
        }
    };
    add("Cargo.toml", rust_checks(root));
    add("package.json", node_checks(root));
    add("python project", python_checks(root));
    add("go.mod", go_checks(root));
    add("composer.json", php_checks(root));
    add("Gemfile", ruby_checks(root));
    add("mix.exs", elixir_checks(root));
    add("Makefile", make_checks(root));
    (checks, origins)
}

fn exists(root: &Path, name: &str) -> bool {
    root.join(name).exists()
}

fn read_small(root: &Path, name: &str) -> Option<String> {
    read_capped(&root.join(name))
}

fn check(kind: CheckKind, command: &str) -> GateCheck {
    GateCheck {
        kind,
        command: command.to_string(),
    }
}

/// Rust. `--workspace` only when the manifest declares one: adding it to a
/// single-crate project is harmless but reads as a command the project does not
/// use, and the gate is quoted back at the model verbatim.
fn rust_checks(root: &Path) -> Vec<GateCheck> {
    let Some(manifest) = read_small(root, "Cargo.toml") else {
        return Vec::new();
    };
    let ws = if manifest.contains("[workspace]") {
        " --workspace"
    } else {
        ""
    };
    vec![
        check(CheckKind::Format, "cargo fmt --all -- --check"),
        check(
            CheckKind::Lint,
            &format!("cargo clippy{ws} --all-targets -- -D warnings"),
        ),
        check(CheckKind::Test, &format!("cargo test{ws}")),
    ]
}

/// Node. The scripts are the gate — a project's `package.json` already says what
/// its checks are called, and inventing `npm run lint` for a project with no
/// `lint` script would hand the model a command that fails with a script-not-
/// found error it then has to diagnose.
fn node_checks(root: &Path) -> Vec<GateCheck> {
    let Some(text) = read_small(root, "package.json") else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) else {
        return Vec::new();
    };
    // The lockfile names the package manager. `npm` last, as the default that
    // needs no evidence.
    let pm = if exists(root, "pnpm-lock.yaml") {
        "pnpm"
    } else if exists(root, "yarn.lock") {
        "yarn"
    } else if exists(root, "bun.lockb") || exists(root, "bun.lock") {
        "bun"
    } else {
        "npm"
    };
    /// Script names that answer a known question, most specific spelling first.
    const NAMED: &[(&str, CheckKind)] = &[
        ("format:check", CheckKind::Format),
        ("fmt:check", CheckKind::Format),
        ("format", CheckKind::Format),
        ("fmt", CheckKind::Format),
        ("lint", CheckKind::Lint),
        ("typecheck", CheckKind::TypeCheck),
        ("type-check", CheckKind::TypeCheck),
        ("types", CheckKind::TypeCheck),
        ("tsc", CheckKind::TypeCheck),
        ("build", CheckKind::Build),
        ("test", CheckKind::Test),
    ];
    let mut out = Vec::new();
    let mut seen: Vec<CheckKind> = Vec::new();
    for (name, kind) in NAMED {
        if !scripts.contains_key(*name) || seen.contains(kind) {
            continue;
        }
        seen.push(*kind);
        out.push(check(*kind, &format!("{pm} run {name}")));
    }
    out
}

/// Python. `pytest` is unconditional once the project looks like Python; the
/// rest are emitted only where the tool is actually configured, since a
/// `ruff check` in a project with no ruff is a command that will not run.
fn python_checks(root: &Path) -> Vec<GateCheck> {
    const MANIFESTS: &[&str] = &[
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "requirements.txt",
        "tox.ini",
    ];
    let mut config = String::new();
    let mut present = false;
    for name in MANIFESTS {
        if let Some(text) = read_small(root, name) {
            present = true;
            config.push_str(&text);
        }
    }
    if !present {
        return Vec::new();
    }
    let mut out = Vec::new();
    if config.contains("ruff") {
        out.push(check(CheckKind::Format, "ruff format --check ."));
        out.push(check(CheckKind::Lint, "ruff check ."));
    } else {
        if config.contains("black") {
            out.push(check(CheckKind::Format, "black --check ."));
        }
        if config.contains("flake8") {
            out.push(check(CheckKind::Lint, "flake8"));
        }
    }
    if config.contains("mypy") {
        out.push(check(CheckKind::TypeCheck, "mypy ."));
    }
    out.push(check(CheckKind::Test, "pytest"));
    out
}

fn go_checks(root: &Path) -> Vec<GateCheck> {
    if !exists(root, "go.mod") {
        return Vec::new();
    }
    let mut out = vec![check(CheckKind::Lint, "go vet ./...")];
    if exists(root, ".golangci.yml") || exists(root, ".golangci.yaml") {
        out.push(check(CheckKind::Lint, "golangci-lint run"));
    }
    out.push(check(CheckKind::Test, "go test ./..."));
    out
}

/// PHP. Which tools are present is read off `require-dev`, because the PHP
/// toolchain is genuinely a choice — phpunit or pest, phpstan or psalm, pint or
/// php-cs-fixer — and every one of them lives at a different `vendor/bin` path.
fn php_checks(root: &Path) -> Vec<GateCheck> {
    let Some(text) = read_small(root, "composer.json") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if text.contains("laravel/pint") {
        out.push(check(CheckKind::Format, "vendor/bin/pint --test"));
    } else if text.contains("php-cs-fixer") {
        out.push(check(
            CheckKind::Format,
            "vendor/bin/php-cs-fixer fix --dry-run",
        ));
    }
    if text.contains("phpstan") {
        out.push(check(CheckKind::Lint, "vendor/bin/phpstan analyse"));
    } else if text.contains("psalm") {
        out.push(check(CheckKind::Lint, "vendor/bin/psalm"));
    }
    if text.contains("pestphp/pest") {
        out.push(check(CheckKind::Test, "vendor/bin/pest"));
    } else {
        out.push(check(CheckKind::Test, "vendor/bin/phpunit"));
    }
    out
}

fn ruby_checks(root: &Path) -> Vec<GateCheck> {
    let Some(text) = read_small(root, "Gemfile") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if text.contains("rubocop") {
        out.push(check(CheckKind::Lint, "bundle exec rubocop"));
    }
    out.push(check(
        CheckKind::Test,
        if text.contains("rspec") {
            "bundle exec rspec"
        } else {
            "bundle exec rake test"
        },
    ));
    out
}

fn elixir_checks(root: &Path) -> Vec<GateCheck> {
    let Some(text) = read_small(root, "mix.exs") else {
        return Vec::new();
    };
    let mut out = vec![check(CheckKind::Format, "mix format --check-formatted")];
    if text.contains("credo") {
        out.push(check(CheckKind::Lint, "mix credo"));
    }
    out.push(check(CheckKind::Test, "mix test"));
    out
}

/// A Makefile is the last resort and the most honest one: whatever targets the
/// project wrote down are what it considers doable, so only targets that exist
/// are offered. Recursive-make variables and pattern rules are ignored — a
/// target name with a `$` or a `%` in it is not something to hand a model.
fn make_checks(root: &Path) -> Vec<GateCheck> {
    const NAMES: &[&str] = &["Makefile", "makefile", "GNUmakefile"];
    let text = NAMES.iter().find_map(|n| read_small(root, n));
    let Some(text) = text else {
        return Vec::new();
    };
    const NAMED: &[(&str, CheckKind)] = &[
        ("fmt", CheckKind::Format),
        ("format", CheckKind::Format),
        ("lint", CheckKind::Lint),
        ("typecheck", CheckKind::TypeCheck),
        ("build", CheckKind::Build),
        ("test", CheckKind::Test),
        ("check", CheckKind::Test),
    ];
    let targets: Vec<&str> = text
        .lines()
        .filter(|l| !l.starts_with(['\t', ' ', '#']))
        .filter_map(|l| l.split_once(':').map(|(name, _)| name.trim()))
        .filter(|name| !name.is_empty() && !name.contains(['$', '%', '=', '(']))
        .collect();
    let mut out = Vec::new();
    let mut seen: Vec<CheckKind> = Vec::new();
    for (name, kind) in NAMED {
        if !targets.contains(name) || seen.contains(kind) {
            continue;
        }
        seen.push(*kind);
        out.push(check(*kind, &format!("make {name}")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, body).expect("write");
    }

    fn commands(gate: &Gate) -> Vec<&str> {
        gate.checks.iter().map(|c| c.command.as_str()).collect()
    }

    #[test]
    fn a_github_actions_workflow_is_the_gate() {
        let dir = TempDir::new().expect("tempdir");
        write(
            dir.path(),
            ".github/workflows/ci.yml",
            r#"
name: ci
on: [push]
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install
        run: rustup component add clippy
      - name: Format
        run: cargo fmt --all -- --check
      - name: Lint
        run: cargo clippy --workspace --all-targets -- -D warnings
      - name: Test
        run: |
          cargo test --workspace
"#,
        );
        // A Cargo.toml is present too: CI must win over the ecosystem guess,
        // because CI is what actually turns the pull request red.
        write(dir.path(), "Cargo.toml", "[workspace]\n");
        let gate = Gate::detect(dir.path());
        assert_eq!(gate.source, Some(GateSource::Ci));
        assert_eq!(gate.origins, vec![".github/workflows/ci.yml".to_string()]);
        assert_eq!(
            commands(&gate),
            vec![
                "cargo fmt --all -- --check",
                "cargo clippy --workspace --all-targets -- -D warnings",
                "cargo test --workspace",
            ],
            "run order, and the setup step is not a check",
        );
    }

    /// The claim this module makes: providers differ in *where* the commands
    /// sit, not in what they look like, so one walk reads all of them. If that
    /// is wrong for a provider, this is where it shows.
    #[test]
    fn every_provider_layout_yields_the_same_gate() {
        let cases: &[(&str, &str)] = &[
            (
                ".gitlab-ci.yml",
                "test:\n  stage: test\n  before_script:\n    - cargo --version\n  script:\n    - cargo test --workspace\n",
            ),
            (
                ".circleci/config.yml",
                "jobs:\n  build:\n    steps:\n      - checkout\n      - run:\n          name: Tests\n          command: cargo test --workspace\n",
            ),
            (
                "azure-pipelines.yml",
                "steps:\n  - script: cargo test --workspace\n    displayName: Tests\n",
            ),
            (
                "bitbucket-pipelines.yml",
                "pipelines:\n  default:\n    - step:\n        script:\n          - cargo test --workspace\n",
            ),
            (
                ".drone.yml",
                "kind: pipeline\nsteps:\n  - name: test\n    commands:\n      - cargo test --workspace\n",
            ),
            (
                ".travis.yml",
                "language: rust\nscript:\n  - cargo test --workspace\n",
            ),
            (
                ".cirrus.yml",
                "task:\n  test_script: cargo test --workspace\n",
            ),
            (
                "Jenkinsfile",
                "pipeline {\n  stages {\n    stage('Test') {\n      steps {\n        sh 'cargo test --workspace'\n      }\n    }\n  }\n}\n",
            ),
        ];
        for (name, body) in cases {
            let dir = TempDir::new().expect("tempdir");
            write(dir.path(), name, body);
            let gate = Gate::detect(dir.path());
            assert_eq!(
                commands(&gate),
                vec!["cargo test --workspace"],
                "{name} should yield the gate"
            );
            assert_eq!(gate.source, Some(GateSource::Ci), "{name}");
        }
    }

    #[test]
    fn a_matrix_expression_is_a_template_not_a_command() {
        let dir = TempDir::new().expect("tempdir");
        write(
            dir.path(),
            ".github/workflows/ci.yml",
            "jobs:\n  t:\n    steps:\n      - run: cargo test ${{ matrix.flags }}\n      - run: cargo fmt --all -- --check\n",
        );
        let gate = Gate::detect(dir.path());
        assert_eq!(
            commands(&gate),
            vec!["cargo fmt --all -- --check"],
            "an uninterpolated expression cannot be run as written",
        );
    }

    #[test]
    fn the_broadest_command_per_kind_survives() {
        let dir = TempDir::new().expect("tempdir");
        // Two workflows, both testing, one narrower. The gate is the bar, so it
        // keeps the whole-tree run.
        write(
            dir.path(),
            ".github/workflows/a.yml",
            "jobs:\n  t:\n    steps:\n      - run: cargo test -p hrdr-app\n",
        );
        write(
            dir.path(),
            ".github/workflows/b.yml",
            "jobs:\n  t:\n    steps:\n      - run: cargo test --workspace\n",
        );
        let gate = Gate::detect(dir.path());
        assert_eq!(commands(&gate), vec!["cargo test --workspace"]);
        assert_eq!(gate.origins.len(), 2, "both files contributed");
    }

    #[test]
    fn ci_with_nothing_recognisable_falls_through_to_the_ecosystem() {
        let dir = TempDir::new().expect("tempdir");
        write(
            dir.path(),
            ".github/workflows/release.yml",
            "jobs:\n  publish:\n    steps:\n      - run: gh release create v1\n",
        );
        write(dir.path(), "Cargo.toml", "[workspace]\nmembers = []\n");
        let gate = Gate::detect(dir.path());
        assert_eq!(gate.source, Some(GateSource::Ecosystem));
        assert_eq!(
            commands(&gate),
            vec![
                "cargo fmt --all -- --check",
                "cargo clippy --workspace --all-targets -- -D warnings",
                "cargo test --workspace",
            ],
        );
    }

    #[test]
    fn a_single_crate_is_not_told_to_run_a_workspace() {
        let dir = TempDir::new().expect("tempdir");
        write(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        let gate = Gate::detect(dir.path());
        assert_eq!(
            commands(&gate),
            vec![
                "cargo fmt --all -- --check",
                "cargo clippy --all-targets -- -D warnings",
                "cargo test",
            ],
        );
    }

    #[test]
    fn node_offers_only_the_scripts_the_project_declares() {
        let dir = TempDir::new().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"scripts": {"test": "vitest", "lint": "eslint .", "dev": "vite"}}"#,
        );
        write(dir.path(), "pnpm-lock.yaml", "lockfileVersion: 9\n");
        let gate = Gate::detect(dir.path());
        assert_eq!(
            commands(&gate),
            vec!["pnpm run lint", "pnpm run test"],
            "no build or typecheck script exists, so neither is offered",
        );
    }

    #[test]
    fn a_polyglot_repo_gates_on_every_ecosystem_it_has() {
        let dir = TempDir::new().expect("tempdir");
        write(dir.path(), "Cargo.toml", "[package]\nname = \"api\"\n");
        write(
            dir.path(),
            "package.json",
            r#"{"scripts":{"test":"vitest"}}"#,
        );
        let gate = Gate::detect(dir.path());
        assert!(commands(&gate).contains(&"cargo test"));
        assert!(commands(&gate).contains(&"npm run test"));
        assert_eq!(
            gate.origins,
            vec!["Cargo.toml".to_string(), "package.json".to_string()],
        );
    }

    #[test]
    fn python_offers_only_the_tools_it_can_see_configured() {
        let dir = TempDir::new().expect("tempdir");
        write(
            dir.path(),
            "pyproject.toml",
            "[tool.ruff]\nline-length = 100\n",
        );
        let gate = Gate::detect(dir.path());
        assert_eq!(
            commands(&gate),
            vec!["ruff format --check .", "ruff check .", "pytest"],
        );

        let bare = TempDir::new().expect("tempdir");
        write(bare.path(), "setup.py", "from setuptools import setup\n");
        assert_eq!(
            commands(&Gate::detect(bare.path())),
            vec!["pytest"],
            "no linter is configured, so none is invented",
        );
    }

    #[test]
    fn a_makefile_offers_only_targets_that_exist() {
        let dir = TempDir::new().expect("tempdir");
        write(
            dir.path(),
            "Makefile",
            "OUT = build\n\ntest:\n\tgo test ./...\n\nlint:\n\tgo vet ./...\n\n%.o: %.c\n\tcc -c $<\n",
        );
        let gate = Gate::detect(dir.path());
        assert_eq!(commands(&gate), vec!["make lint", "make test"]);
    }

    #[test]
    fn nothing_recognisable_is_no_gate_rather_than_a_guess() {
        let dir = TempDir::new().expect("tempdir");
        write(dir.path(), "notes.txt", "hello");
        let gate = Gate::detect(dir.path());
        assert!(gate.is_empty());
        assert_eq!(gate.source, None);
        assert_eq!(gate.matched("cargo test --workspace"), None);
    }

    /// The motivating case: this repo's own lint command names no workspace, so
    /// the classifier calls it partial — but it is what CI runs, so running it
    /// clears the bar.
    #[test]
    fn running_the_gate_counts_as_whole_even_when_the_classifier_would_not() {
        let dir = TempDir::new().expect("tempdir");
        write(
            dir.path(),
            ".gitlab-ci.yml",
            "lint:\n  script:\n    - cargo clippy --all-targets -- -D warnings\n",
        );
        let gate = Gate::detect(dir.path());
        assert_eq!(
            classify("cargo clippy --all-targets -- -D warnings"),
            Some((CheckKind::Lint, Scope::Partial)),
            "precondition: the classifier alone calls this partial",
        );
        assert_eq!(
            gate.matched("cargo clippy --all-targets -- -D warnings"),
            Some((CheckKind::Lint, Scope::Whole)),
        );
        // Extra flags still count — the gate's own tokens are all present.
        assert_eq!(
            gate.matched("cargo clippy --all-targets --all-features -- -D warnings"),
            Some((CheckKind::Lint, Scope::Whole)),
        );
        // Dropping one of them does not.
        assert_eq!(gate.matched("cargo clippy -- -D warnings"), None);
    }

    #[test]
    fn narrowing_a_whole_gate_command_does_not_count_as_whole() {
        let dir = TempDir::new().expect("tempdir");
        write(
            dir.path(),
            ".gitlab-ci.yml",
            "test:\n  script:\n    - pytest\n",
        );
        let gate = Gate::detect(dir.path());
        assert_eq!(
            gate.matched("pytest"),
            Some((CheckKind::Test, Scope::Whole))
        );
        assert_eq!(
            gate.matched("pytest -k slow"),
            Some((CheckKind::Test, Scope::Partial)),
            "the gate is the whole suite; a filtered run is not it",
        );
    }

    #[test]
    fn a_gate_command_is_matched_through_a_wrapper_and_a_chain() {
        let dir = TempDir::new().expect("tempdir");
        write(
            dir.path(),
            ".gitlab-ci.yml",
            "test:\n  script:\n    - cargo test --workspace\n",
        );
        let gate = Gate::detect(dir.path());
        for command in [
            "RUST_LOG=debug cargo test --workspace",
            "cargo fmt --all && cargo test --workspace",
            "cargo test --workspace > out.log",
        ] {
            assert_eq!(
                gate.matched(command).map(|(k, _)| k),
                Some(CheckKind::Test),
                "{command}"
            );
        }
    }

    #[test]
    fn an_unreadably_large_config_is_a_miss_not_a_failure() {
        let dir = TempDir::new().expect("tempdir");
        let mut body = String::from("jobs:\n  t:\n    steps:\n");
        while body.len() as u64 <= MAX_CI_BYTES {
            body.push_str("      - run: cargo test --workspace\n");
        }
        write(dir.path(), ".github/workflows/ci.yml", &body);
        write(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        let gate = Gate::detect(dir.path());
        assert_eq!(
            gate.source,
            Some(GateSource::Ecosystem),
            "the oversized file is skipped, and detection continues",
        );
    }

    #[test]
    fn malformed_yaml_is_skipped_without_taking_the_gate_with_it() {
        let dir = TempDir::new().expect("tempdir");
        write(dir.path(), ".github/workflows/a.yml", "jobs: [\n  broken");
        write(
            dir.path(),
            ".github/workflows/b.yml",
            "jobs:\n  t:\n    steps:\n      - run: cargo test --workspace\n",
        );
        let gate = Gate::detect(dir.path());
        assert_eq!(commands(&gate), vec!["cargo test --workspace"]);
        assert_eq!(gate.origins, vec![".github/workflows/b.yml".to_string()]);
    }

    /// A workflow directory past `MAX_CI_FILES` must not push the single-file
    /// configs out of the list. The cap is a per-directory summary, not a
    /// budget the providers compete for — a repo with seventeen release
    /// workflows and one GitLab pipeline still gates on the GitLab pipeline.
    #[test]
    fn many_workflows_do_not_hide_a_single_file_config() {
        let dir = TempDir::new().expect("tempdir");
        for i in 0..MAX_CI_FILES + 4 {
            write(
                dir.path(),
                &format!(".github/workflows/{i:02}.yml"),
                "jobs:\n  t:\n    steps:\n      - run: echo nothing to see here\n",
            );
        }
        write(
            dir.path(),
            ".gitlab-ci.yml",
            "test:\n  script:\n    - cargo test --workspace\n",
        );
        let gate = Gate::detect(dir.path());
        assert_eq!(gate.source, Some(GateSource::Ci));
        assert_eq!(commands(&gate), vec!["cargo test --workspace"]);
        assert_eq!(gate.origins, vec![".gitlab-ci.yml".to_string()]);
    }
}

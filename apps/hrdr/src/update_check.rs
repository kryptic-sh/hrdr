use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde_json::{Value, json};

const GIT: &str = "git";
const GH: &str = "gh";
const REQUIRED_JOBS: &[&str] = &[
    "rustfmt",
    "clippy ubuntu-latest",
    "clippy macos-latest",
    "clippy windows-latest",
    "cargo-machete (unused deps)",
    "cargo-deny",
    "cargo-audit",
    "test ubuntu-latest",
    "test macos-latest",
    "test windows-latest",
    "no test writes real user state ubuntu-latest",
    "no test writes real user state macos-latest",
    "no test writes real user state windows-latest",
    "build + smoke ubuntu-latest",
    "build + smoke macos-latest",
    "build + smoke windows-latest",
    "Build x86_64-unknown-linux-gnu",
    "Build aarch64-unknown-linux-gnu",
    "Build x86_64-unknown-linux-musl",
    "Build aarch64-unknown-linux-musl",
    "Build x86_64-pc-windows-msvc",
    "Build aarch64-apple-darwin",
    "Build x86_64-apple-darwin",
];
const ALLOWED_SKIPS: &[&str] = &[
    "Publish GitHub Release",
    "Publish to crates.io",
    "Publish AUR (hrdr-bin)",
    "Publish Homebrew formula",
    "Publish Scoop manifest",
    "Publish Alpine .apk",
    "tag-release status",
];

#[derive(Debug, Clone)]
struct ProbeResult {
    completed: bool,
    success: bool,
    stdout: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeCommand {
    Git,
    Gh,
}

trait Probe {
    fn run<'a>(
        &'a self,
        command: ProbeCommand,
        cwd: &'a Path,
        args: &'a [&'a str],
    ) -> Pin<Box<dyn Future<Output = ProbeResult> + 'a>>;
}

struct SystemProbe {
    git: hrdr_tools::CommandRunner,
    gh: hrdr_tools::CommandRunner,
}
impl SystemProbe {
    fn new() -> Self {
        Self {
            git: hrdr_tools::CommandRunner::git(GIT),
            gh: hrdr_tools::CommandRunner::new(GH),
        }
    }
}
impl Probe for SystemProbe {
    fn run<'a>(
        &'a self,
        command: ProbeCommand,
        cwd: &'a Path,
        args: &'a [&'a str],
    ) -> Pin<Box<dyn Future<Output = ProbeResult> + 'a>> {
        Box::pin(async move {
            let result = match command {
                ProbeCommand::Git => self.git.run(cwd, args).await,
                ProbeCommand::Gh => self.gh.run(cwd, args).await,
            };
            ProbeResult {
                completed: result.kind == hrdr_tools::CommandRunKind::Completed,
                success: result.status == Some(0),
                stdout: result.stdout,
            }
        })
    }
}

#[derive(Clone)]
struct GitHubRepo {
    owner: String,
    name: String,
}
impl GitHubRepo {
    fn selector(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

struct RepoView {
    name_with_owner: String,
    is_fork: bool,
    parent_name_with_owner: Option<String>,
}
struct WorkflowRun {
    name: String,
    event: String,
    head_branch: String,
    head_sha: String,
    created_at: String,
    id: u64,
    run_attempt: u64,
    status: String,
    conclusion: Option<String>,
}
struct Job {
    name: String,
    status: String,
    conclusion: Option<String>,
}

pub async fn run_json(cwd: &Path) -> (String, i32) {
    run_with(&SystemProbe::new(), cwd).await
}

async fn run_with(probe: &impl Probe, cwd: &Path) -> (String, i32) {
    let root = probe
        .run(ProbeCommand::Git, cwd, &["rev-parse", "--show-toplevel"])
        .await;
    let Some(root) = output_line(&root) else {
        return unavailable("repository_inspection_unavailable");
    };
    let root = PathBuf::from(root);
    let clean = probe
        .run(
            ProbeCommand::Git,
            &root,
            &["status", "--porcelain=v1", "--untracked-files=all", "-z"],
        )
        .await;
    if !clean.completed || !clean.success {
        return unavailable("working_tree_inspection_unavailable");
    }
    if !clean.stdout.is_empty() {
        return ineligible("working_tree_dirty");
    }
    let branch = probe
        .run(
            ProbeCommand::Git,
            &root,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
        )
        .await;
    if output_line(&branch).as_deref() != Some("main") {
        return ineligible("branch_not_main");
    }
    let head = probe
        .run(
            ProbeCommand::Git,
            &root,
            &["rev-parse", "--verify", "HEAD^{commit}"],
        )
        .await;
    let Some(head) = valid_object_id(output_line(&head).as_deref()) else {
        return unavailable("head_inspection_unavailable");
    };
    let origin = probe
        .run(ProbeCommand::Git, &root, &["remote", "get-url", "origin"])
        .await;
    let Some(origin) = output_line(&origin) else {
        return ineligible("origin_not_configured");
    };
    if origin_has_credentials(&origin) {
        return unavailable("origin_credentials_unsupported");
    }
    let Some(repo) = parse_github_origin(&origin) else {
        return ineligible("origin_incompatible");
    };
    let live = probe
        .run(
            ProbeCommand::Git,
            &root,
            &["ls-remote", "--exit-code", "origin", "refs/heads/main"],
        )
        .await;
    let Some(live) = exact_live_main(&live) else {
        return unavailable("live_main_inspection_unavailable");
    };
    if live != head {
        return ineligible("live_main_not_at_head");
    }
    let selector = repo.selector();
    if selector != "kryptic-sh/hrdr" {
        let view = probe
            .run(
                ProbeCommand::Gh,
                &root,
                &[
                    "repo",
                    "view",
                    "--repo",
                    &selector,
                    "--json",
                    "nameWithOwner,isFork,parent",
                ],
            )
            .await;
        let Some(view) = parse_repo_view(&view) else {
            return unavailable("origin_validation_unavailable");
        };
        if view.name_with_owner != selector
            || !view.is_fork
            || view.parent_name_with_owner.as_deref() != Some("kryptic-sh/hrdr")
        {
            return ineligible("origin_not_approved");
        }
    }
    let endpoint = format!("repos/{selector}/actions/runs?branch=main&event=push&per_page=100");
    let runs = probe
        .run(
            ProbeCommand::Gh,
            &root,
            &["api", "--paginate", "--slurp", &endpoint],
        )
        .await;
    let Some(runs) = parse_runs(&runs) else {
        return unavailable("ci_inspection_unavailable");
    };
    let selected = runs
        .into_iter()
        .filter(|run| {
            run.name == "CI"
                && run.event == "push"
                && run.head_branch == "main"
                && run.head_sha.eq_ignore_ascii_case(&head)
        })
        .max_by(|left, right| {
            (left.created_at.as_str(), left.id).cmp(&(right.created_at.as_str(), right.id))
        });
    let Some(selected) = selected else {
        return ineligible("ci_run_not_found");
    };
    if selected.status != "completed" || selected.conclusion.as_deref() != Some("success") {
        return ineligible("ci_run_not_successful");
    }
    let id = selected.id.to_string();
    let attempt = selected.run_attempt.to_string();
    let details = probe
        .run(
            ProbeCommand::Gh,
            &root,
            &[
                "run",
                "view",
                &id,
                "--attempt",
                &attempt,
                "--repo",
                &selector,
                "--json",
                "jobs",
            ],
        )
        .await;
    let Some(jobs) = parse_jobs(&details) else {
        return unavailable("ci_inspection_unavailable");
    };
    if !jobs_approved(&jobs) {
        return ineligible("ci_jobs_not_approved");
    }
    (json!({"schema_version":1,"eligible":true,"outcome":"eligible","reasons":[],"ci_status":"passed","restart_available":false}).to_string(), 0)
}

fn jobs_approved(jobs: &[Job]) -> bool {
    let mut required = std::collections::BTreeSet::new();
    for job in jobs {
        if REQUIRED_JOBS.contains(&job.name.as_str()) {
            if !required.insert(job.name.as_str())
                || job.status != "completed"
                || job.conclusion.as_deref() != Some("success")
            {
                return false;
            }
        } else if !ALLOWED_SKIPS.contains(&job.name.as_str())
            || job.status != "completed"
            || job.conclusion.as_deref() != Some("skipped")
        {
            return false;
        }
    }
    required.len() == REQUIRED_JOBS.len()
}
fn result(outcome: &str, reason: &str) -> String {
    json!({"schema_version":1,"eligible":false,"outcome":outcome,"reasons":[reason],"ci_status":"not_checked","restart_available":false}).to_string()
}
fn ineligible(reason: &str) -> (String, i32) {
    (result("ineligible", reason), 1)
}
fn unavailable(reason: &str) -> (String, i32) {
    (result("unavailable", reason), 2)
}
fn output_line(result: &ProbeResult) -> Option<String> {
    if !result.completed || !result.success {
        return None;
    }
    let value = std::str::from_utf8(&result.stdout).ok()?.trim();
    (!value.is_empty() && !value.contains(['\r', '\n', '\0'])).then(|| value.to_string())
}
fn json_value(result: &ProbeResult) -> Option<Value> {
    if !result.completed || !result.success {
        return None;
    }
    serde_json::from_slice(&result.stdout).ok()
}
fn object_string(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object.get(key)?.as_str().map(str::to_string)
}
fn parse_repo_view(result: &ProbeResult) -> Option<RepoView> {
    let object = json_value(result)?.as_object()?.clone();
    Some(RepoView {
        name_with_owner: object_string(&object, "nameWithOwner")?,
        is_fork: object.get("isFork")?.as_bool()?,
        parent_name_with_owner: object
            .get("parent")
            .and_then(Value::as_object)
            .and_then(|parent| object_string(parent, "nameWithOwner")),
    })
}
fn parse_run(value: &Value) -> Option<WorkflowRun> {
    let object = value.as_object()?;
    Some(WorkflowRun {
        name: object_string(object, "name")?,
        event: object_string(object, "event")?,
        head_branch: object_string(object, "head_branch")?,
        head_sha: object_string(object, "head_sha")?,
        created_at: object_string(object, "created_at")?,
        id: object.get("id")?.as_u64()?,
        run_attempt: object.get("run_attempt")?.as_u64()?,
        status: object_string(object, "status")?,
        conclusion: object
            .get("conclusion")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}
fn parse_runs(result: &ProbeResult) -> Option<Vec<WorkflowRun>> {
    json_value(result)?
        .as_array()?
        .iter()
        .map(|page| {
            page.as_object()?
                .get("workflow_runs")?
                .as_array()?
                .iter()
                .map(parse_run)
                .collect::<Option<Vec<_>>>()
        })
        .collect::<Option<Vec<_>>>()
        .map(|pages| pages.into_iter().flatten().collect())
}
fn parse_jobs(result: &ProbeResult) -> Option<Vec<Job>> {
    json_value(result)?
        .as_object()?
        .get("jobs")?
        .as_array()?
        .iter()
        .map(|value| {
            let object = value.as_object()?;
            Some(Job {
                name: object_string(object, "name")?,
                status: object_string(object, "status")?,
                conclusion: object
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}
fn valid_object_id(value: Option<&str>) -> Option<String> {
    let value = value?;
    (value.len() == 40 || value.len() == 64)
        .then_some(value)
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
}
fn exact_live_main(result: &ProbeResult) -> Option<String> {
    if !result.completed || !result.success {
        return None;
    }
    let output = std::str::from_utf8(&result.stdout).ok()?;
    let mut lines = output.lines();
    let line = lines.next()?;
    if lines.next().is_some() {
        return None;
    }
    let (hash, reference) = line.split_once('\t')?;
    (reference == "refs/heads/main").then_some(())?;
    valid_object_id(Some(hash))
}
fn origin_has_credentials(origin: &str) -> bool {
    origin.contains("://")
        && origin
            .split_once("://")
            .and_then(|(_, rest)| rest.split('/').next())
            .is_some_and(|authority| authority.contains('@'))
}
fn parse_github_origin(origin: &str) -> Option<GitHubRepo> {
    let path = origin
        .strip_prefix("https://github.com/")
        .or_else(|| origin.strip_prefix("http://github.com/"))
        .or_else(|| origin.strip_prefix("ssh://git@github.com/"))
        .or_else(|| origin.strip_prefix("git@github.com:"))?;
    let mut parts = path.trim_end_matches(".git").split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (parts.next().is_none() && !owner.is_empty() && !name.is_empty()).then(|| GitHubRepo {
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Debug)]
    struct ExpectedProbe {
        command: ProbeCommand,
        args: Vec<String>,
        result: ProbeResult,
    }

    struct ScriptedProbe(std::sync::Mutex<VecDeque<ExpectedProbe>>);

    impl ScriptedProbe {
        fn new(steps: impl IntoIterator<Item = ExpectedProbe>) -> Self {
            Self(std::sync::Mutex::new(steps.into_iter().collect()))
        }

        fn assert_finished(&self) {
            assert!(self.0.lock().unwrap().is_empty(), "unconsumed probe steps");
        }
    }

    impl Probe for ScriptedProbe {
        fn run<'a>(
            &'a self,
            command: ProbeCommand,
            _: &'a Path,
            args: &'a [&'a str],
        ) -> Pin<Box<dyn Future<Output = ProbeResult> + 'a>> {
            Box::pin(async move {
                let expected = self
                    .0
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected probe call");
                assert_eq!(command, expected.command);
                assert_eq!(
                    args,
                    expected.args.iter().map(String::as_str).collect::<Vec<_>>()
                );
                expected.result
            })
        }
    }

    fn ok(value: impl AsRef<[u8]>) -> ProbeResult {
        ProbeResult {
            completed: true,
            success: true,
            stdout: value.as_ref().to_vec(),
        }
    }

    fn expect(command: ProbeCommand, args: &[&str], result: ProbeResult) -> ExpectedProbe {
        ExpectedProbe {
            command,
            args: args.iter().map(ToString::to_string).collect(),
            result,
        }
    }

    fn required_jobs() -> Vec<Value> {
        REQUIRED_JOBS
            .iter()
            .map(|name| json!({"name":name,"status":"completed","conclusion":"success"}))
            .collect()
    }

    fn approved_jobs() -> String {
        let mut jobs = required_jobs();
        jobs.push(json!({"name":"tag-release status","status":"completed","conclusion":"skipped"}));
        json!({"jobs": jobs}).to_string()
    }

    fn base(origin: &str) -> Vec<ExpectedProbe> {
        vec![
            expect(
                ProbeCommand::Git,
                &["rev-parse", "--show-toplevel"],
                ok("/checkout\n"),
            ),
            expect(
                ProbeCommand::Git,
                &["status", "--porcelain=v1", "--untracked-files=all", "-z"],
                ok(""),
            ),
            expect(
                ProbeCommand::Git,
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
                ok("main\n"),
            ),
            expect(
                ProbeCommand::Git,
                &["rev-parse", "--verify", "HEAD^{commit}"],
                ok("0123456789012345678901234567890123456789\n"),
            ),
            expect(
                ProbeCommand::Git,
                &["remote", "get-url", "origin"],
                ok(format!("{origin}\n")),
            ),
            expect(
                ProbeCommand::Git,
                &["ls-remote", "--exit-code", "origin", "refs/heads/main"],
                ok("0123456789012345678901234567890123456789\trefs/heads/main\n"),
            ),
        ]
    }

    fn ci(mut steps: Vec<ExpectedProbe>, runs: &str, jobs: String) -> Vec<ExpectedProbe> {
        steps.push(expect(
            ProbeCommand::Gh,
            &[
                "api",
                "--paginate",
                "--slurp",
                "repos/kryptic-sh/hrdr/actions/runs?branch=main&event=push&per_page=100",
            ],
            ok(runs),
        ));
        steps.push(expect(
            ProbeCommand::Gh,
            &[
                "run",
                "view",
                "77",
                "--attempt",
                "3",
                "--repo",
                "kryptic-sh/hrdr",
                "--json",
                "jobs",
            ],
            ok(jobs),
        ));
        steps
    }

    const RUNS: &str = r#"[
        {"workflow_runs":[
            {"name":"CI","event":"push","head_branch":"main","head_sha":"0123456789012345678901234567890123456789","created_at":"2026-10-10T00:00:00Z","id":99,"run_attempt":1,"status":"completed","conclusion":"success"},
            {"name":"CI","event":"push","head_branch":"main","head_sha":"ffffffffffffffffffffffffffffffffffffffff","created_at":"2026-10-12T00:00:00Z","id":100,"run_attempt":1,"status":"completed","conclusion":"success"}
        ]},
        {"workflow_runs":[
            {"name":"CI","event":"push","head_branch":"main","head_sha":"0123456789012345678901234567890123456789","created_at":"2026-10-11T00:00:00Z","id":42,"run_attempt":2,"status":"completed","conclusion":"success"},
            {"name":"CI","event":"push","head_branch":"main","head_sha":"0123456789012345678901234567890123456789","created_at":"2026-10-11T00:00:00Z","id":77,"run_attempt":3,"status":"completed","conclusion":"success"}
        ]}
    ]"#;

    #[test]
    fn parses_real_github_run_id() {
        let run = parse_run(&json!({
            "name": "CI",
            "event": "push",
            "head_branch": "main",
            "head_sha": "0123456789012345678901234567890123456789",
            "created_at": "2026-10-11T00:00:00Z",
            "id": 12345678901_u64,
            "run_attempt": 1,
            "status": "completed",
            "conclusion": "success"
        }))
        .unwrap();
        assert_eq!(run.id, 12_345_678_901);
    }

    #[tokio::test]
    async fn selects_the_newest_exact_run_across_slurped_pages() {
        let probe = ScriptedProbe::new(ci(
            base("https://github.com/kryptic-sh/hrdr.git"),
            RUNS,
            approved_jobs(),
        ));
        let (output, code) = run_with(&probe, Path::new(".")).await;
        probe.assert_finished();

        assert_eq!(code, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap()["ci_status"],
            "passed"
        );
        assert!(!output.contains("012345"));
    }

    #[tokio::test]
    async fn validated_fork_uses_its_own_repo_for_ci_commands() {
        let mut steps = base("git@github.com:someone/hrdr.git");
        steps.push(expect(
            ProbeCommand::Gh,
            &[
                "repo",
                "view",
                "--repo",
                "someone/hrdr",
                "--json",
                "nameWithOwner,isFork,parent",
            ],
            ok(r#"{"nameWithOwner":"someone/hrdr","isFork":true,"parent":{"nameWithOwner":"kryptic-sh/hrdr"}}"#),
        ));
        steps.push(expect(
            ProbeCommand::Gh,
            &[
                "api",
                "--paginate",
                "--slurp",
                "repos/someone/hrdr/actions/runs?branch=main&event=push&per_page=100",
            ],
            ok(RUNS),
        ));
        steps.push(expect(
            ProbeCommand::Gh,
            &[
                "run",
                "view",
                "77",
                "--attempt",
                "3",
                "--repo",
                "someone/hrdr",
                "--json",
                "jobs",
            ],
            ok(approved_jobs()),
        ));
        let probe = ScriptedProbe::new(steps);
        let (_, code) = run_with(&probe, Path::new(".")).await;
        probe.assert_finished();
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn invalid_fork_and_dirty_tree_are_rejected_without_leaks() {
        let mut invalid = base("https://github.com/nope/hrdr.git");
        invalid.push(expect(
            ProbeCommand::Gh,
            &[
                "repo",
                "view",
                "--repo",
                "nope/hrdr",
                "--json",
                "nameWithOwner,isFork,parent",
            ],
            ok(r#"{"nameWithOwner":"nope/hrdr","isFork":false}"#),
        ));
        let probe = ScriptedProbe::new(invalid);
        let (output, code) = run_with(&probe, Path::new(".")).await;
        probe.assert_finished();
        assert_eq!(code, 1);
        assert!(!output.contains("nope"));

        let mut dirty = base("https://github.com/kryptic-sh/hrdr.git");
        dirty[1] = expect(
            ProbeCommand::Git,
            &["status", "--porcelain=v1", "--untracked-files=all", "-z"],
            ok("M secret-token\0"),
        );
        dirty.truncate(2);
        let probe = ScriptedProbe::new(dirty);
        let (output, _) = run_with(&probe, Path::new(".")).await;
        probe.assert_finished();
        assert!(!output.contains("secret"));
    }

    #[tokio::test]
    async fn rejects_failed_duplicate_unknown_and_missing_jobs() {
        for extra in [
            json!({"name":"rustfmt","status":"completed","conclusion":"failure"}),
            json!({"name":"rustfmt","status":"completed","conclusion":"success"}),
            json!({"name":"unknown","status":"completed","conclusion":"success"}),
        ] {
            let mut jobs = required_jobs();
            jobs.push(extra);
            let probe = ScriptedProbe::new(ci(
                base("https://github.com/kryptic-sh/hrdr.git"),
                RUNS,
                json!({"jobs": jobs}).to_string(),
            ));
            let (_, code) = run_with(&probe, Path::new(".")).await;
            probe.assert_finished();
            assert_eq!(code, 1);
        }

        let mut jobs = required_jobs();
        jobs.pop();
        let probe = ScriptedProbe::new(ci(
            base("https://github.com/kryptic-sh/hrdr.git"),
            RUNS,
            json!({"jobs": jobs}).to_string(),
        ));
        let (_, code) = run_with(&probe, Path::new(".")).await;
        probe.assert_finished();
        assert_eq!(code, 1);
    }
}

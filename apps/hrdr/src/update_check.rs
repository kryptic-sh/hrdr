use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde_json::json;

const GIT: &str = "git";

#[derive(Debug, Clone)]
struct ProbeResult {
    completed: bool,
    success: bool,
    stdout: Vec<u8>,
}

trait Probe {
    fn run<'a>(
        &'a self,
        cwd: &'a Path,
        args: &'a [&'a str],
    ) -> Pin<Box<dyn Future<Output = ProbeResult> + 'a>>;
}

struct GitProbe {
    runner: hrdr_tools::GitRunner,
}

impl GitProbe {
    fn new() -> Self {
        Self {
            runner: hrdr_tools::GitRunner::new(GIT),
        }
    }
}

impl Probe for GitProbe {
    fn run<'a>(
        &'a self,
        cwd: &'a Path,
        args: &'a [&'a str],
    ) -> Pin<Box<dyn Future<Output = ProbeResult> + 'a>> {
        Box::pin(async move {
            let result = self.runner.run(cwd, args).await;
            ProbeResult {
                completed: result.kind == hrdr_tools::GitRunKind::Completed,
                success: result.status == Some(0),
                stdout: result.stdout,
            }
        })
    }
}

pub async fn run_json(cwd: &Path) -> (String, i32) {
    run_with(&GitProbe::new(), cwd).await
}

async fn run_with(probe: &impl Probe, cwd: &Path) -> (String, i32) {
    let root = probe.run(cwd, &["rev-parse", "--show-toplevel"]).await;
    let Some(root) = output_line(&root) else {
        return unavailable("repository_inspection_unavailable");
    };
    let root = PathBuf::from(root);

    let clean = probe
        .run(
            &root,
            &["status", "--porcelain=v1", "--untracked-files=all", "-z"],
        )
        .await;
    if !clean.completed {
        return unavailable("working_tree_inspection_unavailable");
    }
    if !clean.success {
        return unavailable("working_tree_inspection_unavailable");
    }
    if !clean.stdout.is_empty() {
        return ineligible("working_tree_dirty");
    }

    let branch = probe
        .run(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await;
    let Some(branch) = output_line(&branch) else {
        return ineligible("branch_not_main");
    };
    if branch != "main" {
        return ineligible("branch_not_main");
    }

    let head = probe
        .run(&root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .await;
    let Some(head) = valid_object_id(output_line(&head).as_deref()) else {
        return unavailable("head_inspection_unavailable");
    };

    let origin = probe.run(&root, &["remote", "get-url", "origin"]).await;
    let Some(origin) = output_line(&origin) else {
        return ineligible("origin_not_configured");
    };
    if origin_has_credentials(&origin) {
        return unavailable("origin_credentials_unsupported");
    }
    if !supported_github_origin(&origin) {
        return ineligible("origin_incompatible");
    }

    let live = probe
        .run(
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

    // Local/source admission alone is deliberately insufficient: CI authorization
    // is added in the second half of the diagnostic.
    ineligible("ci_validation_not_implemented")
}

fn result(outcome: &str, reason: &str) -> String {
    json!({
        "schema_version": 1,
        "eligible": false,
        "outcome": outcome,
        "reasons": [reason],
        "ci_status": "not_checked",
        "restart_available": false,
    })
    .to_string()
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

fn supported_github_origin(origin: &str) -> bool {
    let path = origin
        .strip_prefix("https://github.com/")
        .or_else(|| origin.strip_prefix("http://github.com/"))
        .or_else(|| origin.strip_prefix("ssh://git@github.com/"))
        .or_else(|| origin.strip_prefix("git@github.com:"));
    let Some(path) = path else {
        return false;
    };
    let mut parts = path.trim_end_matches(".git").split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(owner), Some(repository), None) if !owner.is_empty() && !repository.is_empty()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct CannedProbe(std::sync::Mutex<VecDeque<ProbeResult>>);

    impl CannedProbe {
        fn new(results: impl IntoIterator<Item = ProbeResult>) -> Self {
            Self(std::sync::Mutex::new(results.into_iter().collect()))
        }
    }

    impl Probe for CannedProbe {
        fn run<'a>(
            &'a self,
            _: &'a Path,
            _: &'a [&'a str],
        ) -> Pin<Box<dyn Future<Output = ProbeResult> + 'a>> {
            Box::pin(async move { self.0.lock().unwrap().pop_front().unwrap() })
        }
    }

    fn ok(stdout: &str) -> ProbeResult {
        ProbeResult {
            completed: true,
            success: true,
            stdout: stdout.as_bytes().to_vec(),
        }
    }

    fn valid_results() -> Vec<ProbeResult> {
        vec![
            ok("/checkout\n"),
            ok(""),
            ok("main\n"),
            ok("0123456789012345678901234567890123456789\n"),
            ok("https://github.com/kryptic-sh/hrdr.git\n"),
            ok("0123456789012345678901234567890123456789\trefs/heads/main\n"),
        ]
    }

    #[tokio::test]
    async fn clean_main_at_live_head_stays_ineligible_until_ci_exists() {
        let (output, code) = run_with(&CannedProbe::new(valid_results()), Path::new(".")).await;
        assert_eq!(code, 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&output).unwrap()["reasons"],
            json!(["ci_validation_not_implemented"])
        );
    }

    #[tokio::test]
    async fn dirty_non_main_and_stale_remote_are_rejected() {
        let mut dirty = valid_results();
        dirty[1] = ok(" M src/main.rs\0");
        assert_eq!(
            run_with(&CannedProbe::new(dirty), Path::new(".")).await.1,
            1
        );
        let mut branch = valid_results();
        branch[2] = ok("feature\n");
        assert_eq!(
            run_with(&CannedProbe::new(branch), Path::new(".")).await.1,
            1
        );
        let mut stale = valid_results();
        stale[5] = ok("ffffffffffffffffffffffffffffffffffffffff\trefs/heads/main\n");
        assert_eq!(
            run_with(&CannedProbe::new(stale), Path::new(".")).await.1,
            1
        );
    }

    #[tokio::test]
    async fn github_fork_origin_defers_compatibility_until_ci_validation() {
        let mut fork = valid_results();
        fork[4] = ok("git@github.com:someone/hrdr-fork.git\n");
        let (output, code) = run_with(&CannedProbe::new(fork), Path::new(".")).await;
        assert_eq!(code, 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&output).unwrap()["reasons"],
            json!(["ci_validation_not_implemented"])
        );
    }

    #[tokio::test]
    async fn unavailable_and_credential_origins_do_not_leak_raw_data() {
        let unavailable = ProbeResult {
            completed: false,
            success: false,
            stdout: b"secret failure".to_vec(),
        };
        let (output, code) = run_with(&CannedProbe::new([unavailable]), Path::new(".")).await;
        assert_eq!(code, 2);
        assert!(!output.contains("secret failure"));
        let mut credential = valid_results();
        credential[4] = ok("https://user:secret@github.com/kryptic-sh/hrdr.git\n");
        let (output, code) = run_with(&CannedProbe::new(credential), Path::new(".")).await;
        assert_eq!(code, 2);
        assert!(!output.contains("secret"));
    }
}

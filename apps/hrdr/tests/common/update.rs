//! Fixture pieces the self-update end-to-end tests share across platforms: the
//! CI the admission check is shown, the conversation that asks for the update,
//! and the snapshot it leaves.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use super::{Chat, stop_chunk, tool_args_chunk, tool_calls_stop_chunk, tool_start_chunk};

pub const REQUIRED_JOBS: &[&str] = &[
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

/// What `gh` answers for the admitted commit: the run list, then that run's
/// jobs, every required one green.
pub fn ci_responses(head: &str) -> (String, String) {
    let jobs = REQUIRED_JOBS
        .iter()
        .map(|name| format!(r#"{{"name":"{name}","status":"completed","conclusion":"success"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    (
        format!(
            r#"[{{"workflow_runs":[{{"name":"CI","event":"push","head_branch":"main","head_sha":"{head}","created_at":"2026-10-11T00:00:00Z","id":1,"run_attempt":1,"status":"completed","conclusion":"success"}}]}}]"#
        ),
        format!(r#"{{"jobs":[{jobs}]}}"#),
    )
}

pub fn update_conversation() -> Vec<Chat> {
    vec![
        Chat::Sse(vec![
            tool_start_chunk("goal", "goal-add", "goal"),
            tool_args_chunk("goal", r#"{"op":"add","content":"cancel after recovery"}"#),
            tool_calls_stop_chunk("goal"),
            "[DONE]".to_owned(),
        ]),
        Chat::Sse(vec![
            tool_start_chunk("goal", "goal-cancel", "goal"),
            tool_args_chunk("goal", r#"{"op":"cancel","id":1}"#),
            tool_calls_stop_chunk("goal"),
            "[DONE]".to_owned(),
        ]),
        Chat::Sse(vec![
            tool_start_chunk("todo", "todo-complete", "todo"),
            tool_args_chunk(
                "todo",
                r#"{"todos":[{"content":"preserve update state","status":"completed","evidence":"Windows update E2E"}]}"#,
            ),
            tool_calls_stop_chunk("todo"),
            "[DONE]".to_owned(),
        ]),
        Chat::Sse(vec![
            tool_start_chunk("update", "update-call", "update"),
            tool_args_chunk("update", "{}"),
            tool_calls_stop_chunk("update"),
            "[DONE]".to_owned(),
        ]),
        Chat::Sse(vec![
            super::text_chunk("continued", "turn-after-update"),
            stop_chunk("continued"),
            "[DONE]".to_owned(),
        ]),
        Chat::Sse(vec![
            tool_start_chunk("shell", "recovery-cwd", "shell"),
            tool_args_chunk(
                "shell",
                // bash on every platform (Git Bash on Windows). The marker is
                // assembled at run time, so a shell that rejects the command and
                // echoes it back cannot print it: only a run in the session's
                // `apps/hrdr` directory, where `src/main.rs` exists, does.
                r#"{"shell":"bash","command":"test -f src/main.rs && echo recovery-cwd-\"preserved\""}"#,
            ),
            tool_calls_stop_chunk("shell"),
            "[DONE]".to_owned(),
        ]),
        Chat::Sse(vec![
            super::text_chunk("recovered", "continued-after-update"),
            stop_chunk("recovered"),
            "[DONE]".to_owned(),
        ]),
    ]
}

pub fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create Cargo cache directory");
    for entry in fs::read_dir(source).expect("read Cargo cache") {
        let entry = entry.expect("Cargo cache entry");
        let target = destination.join(entry.file_name());
        let file_type = entry.file_type().expect("Cargo cache entry type");
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target);
        } else if file_type.is_file() {
            fs::copy(entry.path(), target).expect("copy Cargo cache file");
        }
    }
}

/// The session snapshot saved for `cwd` under `home`: sessions are filed by
/// cwd, so this also checks the session was saved where it was working — a
/// save filed under another directory leaves this `None`.
pub fn persisted_session_state(home: &Path, cwd: &Path) -> Option<serde_json::Value> {
    let cwd = hrdr_tools::canonical_cwd(cwd).ok()?;
    let directory = home
        .join("hrdr")
        .join("sessions")
        .join(hrdr_agent::cwd_slug(&cwd.display().to_string()));
    let snapshot = fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
        })?;
    serde_json::from_slice(&fs::read(snapshot.path()).ok()?).ok()
}

/// Clone this repository's HEAD into `checkout` and commit the working tree on
/// top of it — tracked changes and untracked, non-ignored files alike — so the
/// fixture installs exactly the source the test binary was built from. `who`
/// names the fixture in the commit's identity.
pub fn clone_working_tree(checkout: &Path, who: &str) {
    let repository = git(None, &["rev-parse", "--show-toplevel"]);
    let repository = Path::new(&repository);
    let head = git(None, &["rev-parse", "--verify", "HEAD^{commit}"]);
    git(
        None,
        &[
            "clone",
            "--no-local",
            "--no-checkout",
            repository.to_str().expect("utf-8 repository path"),
            checkout.to_str().expect("utf-8 checkout path"),
        ],
    );
    git(Some(checkout), &["checkout", "--detach", &head]);

    let patch = Command::new("git")
        .current_dir(repository)
        .args(["diff", "--binary", "HEAD"])
        .output()
        .expect("capture working-tree patch");
    assert!(
        patch.status.success(),
        "git diff failed: {}",
        String::from_utf8_lossy(&patch.stderr)
    );
    if !patch.stdout.is_empty() {
        let mut apply = Command::new("git")
            .current_dir(checkout)
            .args(["apply", "--whitespace=nowarn", "--binary"])
            .stdin(Stdio::piped())
            .spawn()
            .expect("start applying working-tree patch");
        apply
            .stdin
            .take()
            .expect("working-tree patch stdin")
            .write_all(&patch.stdout)
            .expect("write working-tree patch");
        let status = apply.wait().expect("finish applying working-tree patch");
        assert!(status.success(), "applying the working-tree patch failed");
    }
    // A new source file is untracked until it is committed, and a diff does
    // not carry it; without it the clone would not even compile.
    let untracked = git(
        Some(repository),
        &["ls-files", "--others", "--exclude-standard", "-z"],
    );
    for path in untracked.split('\0').filter(|path| !path.is_empty()) {
        let target = checkout.join(path);
        fs::create_dir_all(target.parent().expect("untracked file parent"))
            .expect("create untracked file directory");
        fs::copy(repository.join(path), target).expect("copy untracked file");
    }

    git(Some(checkout), &["add", "--all"]);
    if !git(Some(checkout), &["status", "--porcelain"]).is_empty() {
        git(
            Some(checkout),
            &[
                "-c",
                &format!("user.name=hrdr {who} update E2E"),
                "-c",
                &format!("user.email={who}-update-e2e@invalid"),
                "commit",
                "--no-gpg-sign",
                "-m",
                &format!("{who} update E2E working tree"),
            ],
        );
    }
}

fn git(directory: Option<&Path>, args: &[&str]) -> String {
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    let output = command.args(args).output().expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf-8 git output")
        .trim()
        .to_owned()
}

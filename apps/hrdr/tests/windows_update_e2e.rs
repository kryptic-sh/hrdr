#![cfg(windows)]

// The Windows updater cannot be covered by an in-process test: it replaces the
// executable that is currently running. These ignored tests instead install the
// test binary into a private Cargo home and drive its real TUI through ConPTY.
extern crate hrdr_test_support;

mod common;

use common::{
    Chat, MockServer, stop_chunk, tool_args_chunk, tool_calls_stop_chunk, tool_start_chunk,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(100);
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

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    cargo_home: PathBuf,
    checkout: PathBuf,
    expected_commit: String,
    git_log: PathBuf,
    cargo_log: PathBuf,
    installed: PathBuf,
    old_binary: Vec<u8>,
    env: Vec<(OsString, OsString)>,
}

impl Fixture {
    fn new(fail_cargo_install: bool) -> Self {
        let root = tempfile::tempdir().expect("private update fixture root");
        let home = root.path().join("home");
        let cargo_home = root.path().join("cargo-home");
        let checkout = root.path().join("checkout");
        let bin = root.path().join("bin");
        fs::create_dir_all(&home).expect("private home");
        fs::create_dir_all(cargo_home.join("bin")).expect("private cargo bin");
        fs::create_dir_all(&bin).expect("fixture command bin");

        // Cargo itself must not touch the runner cache after the update child has
        // started. Copy only dependency caches, never Cargo credentials/config.
        let source_cargo = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".cargo"))
            })
            .expect("CARGO_HOME or USERPROFILE");
        for name in ["registry", "git"] {
            let source = source_cargo.join(name);
            if source.exists() {
                copy_tree(&source, &cargo_home.join(name));
            }
        }

        let installed = cargo_home.join("bin/hrdr.exe");
        fs::copy(env!("CARGO_BIN_EXE_hrdr"), &installed).expect("install test binary");
        let old_binary = fs::read(&installed).expect("read installed binary");

        let repository = PathBuf::from(git_output(&["rev-parse", "--show-toplevel"]));
        let working_tree_patch = Command::new("git")
            .args(["diff", "--binary", "HEAD"])
            .output()
            .expect("capture working-tree patch");
        assert!(
            working_tree_patch.status.success(),
            "git diff failed: {}",
            String::from_utf8_lossy(&working_tree_patch.stderr)
        );
        let head = git_output(&["rev-parse", "--verify", "HEAD^{commit}"]);
        run_git(&[
            "clone",
            "--no-local",
            "--no-checkout",
            repository.to_str().expect("utf-8 repository path"),
            checkout.to_str().expect("utf-8 checkout path"),
        ]);
        run_git_in(&checkout, &["checkout", "--detach", &head]);
        if !working_tree_patch.stdout.is_empty() {
            let mut apply = Command::new("git")
                .current_dir(&checkout)
                .args(["apply", "--whitespace=nowarn", "--binary"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("start applying working-tree patch");
            apply
                .stdin
                .take()
                .expect("working-tree patch stdin")
                .write_all(&working_tree_patch.stdout)
                .expect("apply working-tree patch");
            let status = apply.wait().expect("finish applying working-tree patch");
            assert!(
                status.success(),
                "apply working-tree patch failed: {status:?}"
            );
            run_git_in(
                &checkout,
                &[
                    "-c",
                    "user.name=hrdr Windows update E2E",
                    "-c",
                    "user.email=windows-update-e2e@invalid",
                    "commit",
                    "--no-gpg-sign",
                    "-am",
                    "Windows update E2E working tree",
                ],
            );
        }
        let head = git_output_in(&checkout, &["rev-parse", "--verify", "HEAD^{commit}"]);
        run_git_in(&checkout, &["switch", "-C", "main", &head]);
        run_git_in(
            &checkout,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/kryptic-sh/hrdr.git",
            ],
        );

        let real_git = git_program();
        let git_log = bin.join("git.log");
        fs::write(
            bin.join("git.cmd"),
            format!(
                "@echo off\r\nsetlocal EnableDelayedExpansion\r\nset \"log={}\"\r\n>>\"%log%\" echo === git invocation ===\r\n>>\"%log%\" echo cwd: %CD%\r\n>>\"%log%\" echo argv: %*\r\nif \"%~1\"==\"ls-remote\" (\r\n  echo {head}\trefs/heads/main\r\n  set \"code=0\"\r\n) else (\r\n  \"{}\" %*\r\n  set \"code=!ERRORLEVEL!\"\r\n)\r\n>>\"%log%\" echo exit: !code!\r\nexit /b !code!\r\n",
                git_log.display(),
                real_git.display(),
            ),
        )
        .expect("write git fixture");
        let real_cargo = cargo_program();
        let real_git_directory = real_git.parent().expect("git.exe parent");
        let cargo_log = bin.join("cargo.log");
        let cargo_stdout = bin.join("cargo-stdout.log");
        let cargo_stderr = bin.join("cargo-stderr.log");
        fs::write(
            bin.join("cargo.cmd"),
            format!(
                "@echo off\r\nsetlocal EnableDelayedExpansion\r\nset \"log={}\"\r\nset \"stdout={}\"\r\nset \"stderr={}\"\r\nset \"PATH={};%PATH%\"\r\n>>\"%log%\" echo === cargo invocation ===\r\n>>\"%log%\" echo cwd: %CD%\r\n>>\"%log%\" echo argv: %*\r\nif \"{fail_cargo_install}\"==\"true\" if \"%~1\"==\"install\" (\r\n  >>\"%log%\" echo forced install failure\r\n  exit /b 1\r\n)\r\n\"{}\" %* >\"%stdout%\" 2>\"%stderr%\"\r\nset \"code=!ERRORLEVEL!\"\r\n>>\"%log%\" echo stdout:\r\n>>\"%log%\" type \"%stdout%\"\r\n>>\"%log%\" echo stderr:\r\n>>\"%log%\" type \"%stderr%\"\r\n>>\"%log%\" echo exit: !code!\r\ntype \"%stdout%\"\r\ntype \"%stderr%\" >&2\r\nexit /b !code!\r\n",
                cargo_log.display(),
                cargo_stdout.display(),
                cargo_stderr.display(),
                real_git_directory.display(),
                real_cargo.display(),
            ),
        )
        .expect("write cargo fixture");
        fs::write(bin.join("gh.cmd"), gh_script(&head)).expect("write gh fixture");

        let inherited_path = std::env::var_os("PATH").expect("PATH");
        let path = std::env::join_paths([bin.as_os_str(), inherited_path.as_os_str()])
            .expect("fixture PATH");
        let runtime = root.path().join("runtime");
        fs::create_dir_all(&runtime).expect("private runtime");
        let env = [
            ("CARGO_HOME", cargo_home.as_os_str()),
            ("CARGO_NET_OFFLINE", std::ffi::OsStr::new("true")),
            ("HOME", home.as_os_str()),
            ("USERPROFILE", home.as_os_str()),
            ("APPDATA", home.as_os_str()),
            ("LOCALAPPDATA", home.as_os_str()),
            ("XDG_CONFIG_HOME", home.as_os_str()),
            ("XDG_DATA_HOME", home.as_os_str()),
            ("XDG_STATE_HOME", home.as_os_str()),
            ("XDG_CACHE_HOME", home.as_os_str()),
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            ("PATH", path.as_os_str()),
        ]
        .into_iter()
        .map(|(key, value)| (OsString::from(key), value.to_os_string()))
        .collect();

        Self {
            _root: root,
            home,
            cargo_home,
            checkout,
            expected_commit: head,
            git_log,
            cargo_log,
            installed,
            old_binary,
            env,
        }
    }

    fn checkout_status(&self) -> String {
        let output = Command::new("git")
            .current_dir(&self.checkout)
            .args(["status", "--porcelain=v1", "--untracked-files=normal"])
            .output()
            .expect("read fixture checkout status");
        assert!(
            output.status.success(),
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf-8 fixture checkout status")
    }

    fn checkout_head(&self) -> String {
        let output = Command::new("git")
            .current_dir(&self.checkout)
            .args(["rev-parse", "--verify", "HEAD"])
            .output()
            .expect("read fixture checkout head");
        assert!(
            output.status.success(),
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("utf-8 fixture checkout head")
            .trim()
            .to_owned()
    }

    fn assert_checkout_clean(&self, boundary: &str) {
        let head = self.checkout_head();
        let status = self.checkout_status();
        assert_eq!(
            head, self.expected_commit,
            "fixture checkout changed commit at {boundary}"
        );
        assert!(
            status.is_empty(),
            "fixture checkout is dirty at {boundary}:\n{status}"
        );
    }

    fn expected_build_info(&self) -> String {
        format!(
            "version: {}\ncommit: {}\ntarget: {}\nsource: clean\n",
            env!("CARGO_PKG_VERSION"),
            self.expected_commit,
            env!("HRDR_BUILD_TARGET"),
        )
    }

    fn operation_diagnostic(&self) -> String {
        let operations = fs::read_dir(self.cargo_home.join("bin"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with(".hrdr-update-"))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let Some(operation) = operations.first() else {
            return "no update operation directory".to_owned();
        };
        let entries = fs::read_dir(operation)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let request = fs::read_dir(operation)
            .ok()
            .and_then(|mut entries| {
                entries.find_map(|entry| {
                    let path = entry.ok()?.path();
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("request-"))
                        .then_some(path)
                })
            })
            .and_then(|path| fs::read_to_string(path).ok())
            .unwrap_or_else(|| "request was consumed or unavailable".to_owned());
        let operation_hash = operation
            .file_name()
            .expect("operation name")
            .to_string_lossy()
            .trim_start_matches(".hrdr-update-")
            .to_owned();
        let candidate = operation.join(format!("coordinator-{operation_hash}.exe"));
        let output = Command::new(&self.installed).arg("--build-info").output();
        format!(
            "operation: {}\nentries:\n{entries}\nrequest:\n{request}\ncoordinator: {}\ncoordinator exists: {}\ninstalled build info: {output:?}",
            operation.display(),
            candidate.display(),
            candidate.exists(),
        )
    }

    fn start(&self, server: &MockServer) -> Session {
        common::write_config(&self.home, &server.base_url());
        trust(&self.home, &self.checkout);
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open ConPTY");
        let mut command = CommandBuilder::new(&self.installed);
        command.args(["--no-auto-resume", "--no-bell"]);
        command.cwd(&self.checkout);
        command.env("TERM", "xterm-256color");
        command.env_remove("HRDR_MODEL");
        command.env_remove("HRDR_API_KEY");
        command.env_remove("RUST_LOG");
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let child = pty
            .slave
            .spawn_command(command)
            .expect("spawn installed hrdr");
        drop(pty.slave);
        let reader = pty.master.try_clone_reader().expect("ConPTY reader");
        let writer = Arc::new(Mutex::new(pty.master.take_writer().expect("ConPTY writer")));
        let screen = common::drain_pty(reader, Arc::clone(&writer));
        Session {
            child,
            _master: pty.master,
            writer,
            screen,
        }
    }

    fn operation_root(&self) -> PathBuf {
        wait_until("update operation", || {
            fs::read_dir(self.cargo_home.join("bin"))
                .ok()?
                .find_map(|entry| {
                    let path = entry.ok()?.path();
                    let is_operation = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(".hrdr-update-"));
                    is_operation.then_some(path)
                })
        })
    }
}

struct Session {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Writer,
    screen: Arc<Mutex<String>>,
}

impl Session {
    fn send(&self, text: &str) {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        writer.write_all(text.as_bytes()).expect("write ConPTY");
        writer.flush().expect("flush ConPTY");
    }

    fn screen(&self) -> String {
        common::visible(
            &self
                .screen
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }

    fn wait_for(&mut self, needle: &str) {
        let start = Instant::now();
        while !self.screen().contains(needle) {
            if let Some(status) = self.child.try_wait().expect("poll old process") {
                panic!(
                    "old process exited {status:?} before {needle:?}:\n{}",
                    self.screen()
                );
            }
            assert!(
                start.elapsed() < WAIT,
                "timed out waiting for {needle:?}:\n{}",
                self.screen()
            );
            std::thread::sleep(POLL);
        }
    }

    fn wait_for_exit(&mut self, fixture: &Fixture) -> Duration {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("poll old process") {
                assert!(
                    status.success(),
                    "old installed process exited unsuccessfully {status:?}:\n{}",
                    self.screen()
                );
                return start.elapsed();
            }
            let screen = self.screen();
            assert!(
                !screen.contains("update preparation failed"),
                "update preparation failed; expected checkout head: {}\nactual checkout head: {}\nfixture checkout status:\n{}\nfixture git log:\n{}\nfixture Cargo log:\n{}\nfixture operation:\n{}\ncoordinator output / recovery screen:\n{screen}",
                fixture.expected_commit,
                fixture.checkout_head(),
                fixture.checkout_status(),
                fs::read_to_string(&fixture.git_log).unwrap_or_default(),
                fs::read_to_string(&fixture.cargo_log).unwrap_or_default(),
                fixture.operation_diagnostic(),
            );
            assert!(
                start.elapsed() < WAIT,
                "old installed process did not exit; expected checkout head: {}\nactual checkout head: {}\nfixture checkout status:\n{}\nfixture operation:\n{}\ncoordinator output / recovery screen:\n{screen}",
                fixture.expected_commit,
                fixture.checkout_head(),
                fixture.checkout_status(),
                fixture.operation_diagnostic(),
            );
            std::thread::sleep(POLL);
        }
    }
}

#[test]
#[ignore = "replaces an executable; run only in Windows release CI"]
fn update_installs_default_cargo_binary_and_recovers_once() {
    let server = MockServer::start(update_conversation());
    let fixture = Fixture::new(false);
    fixture.assert_checkout_clean("fixture setup");
    let mut session = fixture.start(&server);
    session.wait_for("mock-model");
    fixture.assert_checkout_clean("initial TUI startup");
    session.send("/cwd apps/hrdr\r");
    session.wait_for("apps/hrdr");
    session.send("install update\r");
    wait_until("tool-result continuation before handoff", || {
        (server.chat_bodies().len() == 5 && session.screen().contains("turn-after-update"))
            .then_some(())
    });
    let _ = session.wait_for_exit(&fixture);
    fixture.assert_checkout_clean("update handoff");

    let installed_build_info = wait_until("default Cargo install", || {
        let bytes = fs::read(&fixture.installed).ok()?;
        (bytes != fixture.old_binary)
            .then(|| {
                Command::new(&fixture.installed)
                    .arg("--build-info")
                    .output()
                    .ok()
            })
            .flatten()
            .filter(|output| output.status.success())
    });
    assert_eq!(
        String::from_utf8(installed_build_info.stdout).expect("UTF-8 installed build info"),
        fixture.expected_build_info(),
        "the default CARGO_HOME/bin/hrdr.exe must attest the admitted checkout"
    );
    let cargo_log = fs::read_to_string(&fixture.cargo_log).expect("read fixture Cargo log");
    assert!(
        cargo_log.contains("argv: install --path apps/hrdr --locked --force --bin hrdr"),
        "the coordinator must run Cargo install against the admitted checkout:\n{cargo_log}"
    );

    wait_until("recovered shell cwd", || {
        let bodies = server.chat_bodies();
        (bodies.len() == 7
            && session.screen().contains("continued-after-update")
            && bodies
                .last()
                .is_some_and(|body| body.to_string().contains("recovery-cwd-preserved")))
        .then_some(())
    });
    assert_eq!(
        server.chat_bodies().len(),
        7,
        "the persisted update turn and one recovered shell continuation must run"
    );
    let persisted = wait_until("persisted recovered state", || {
        let state = persisted_session_state(&fixture.home)?;
        let messages = state["messages"].as_array()?;
        let todos = state["todos"].as_array()?;
        let goals = state["goals"].as_array()?;
        (messages
            .iter()
            .any(|message| message.to_string().contains("install update"))
            && todos.iter().any(|todo| {
                todo["content"] == "preserve update state"
                    && todo["status"] == "completed"
                    && todo["evidence"] == "Windows update E2E"
            })
            && goals.iter().any(|goal| {
                goal["content"] == "cancel after recovery" && goal["status"] == "cancelled"
            }))
        .then_some(state)
    });
    assert!(
        persisted["messages"]
            .as_array()
            .is_some_and(|messages| !messages.is_empty())
    );
    fixture.assert_checkout_clean("recovered continuation");
}

#[test]
#[ignore = "replaces an executable; run only in Windows release CI"]
fn reused_request_does_not_install_or_recover_again() {
    let server = MockServer::start(update_conversation());
    let fixture = Fixture::new(false);
    let mut session = fixture.start(&server);
    session.wait_for("mock-model");
    session.send("install update\r");
    let _ = session.wait_for_exit(&fixture);
    let operation = fixture.operation_root();
    wait_until("recovered continuation", || {
        (server.chat_bodies().len() == 7).then_some(())
    });
    let destination = fs::read(&fixture.installed).expect("installed destination");
    let coordinator = single(&operation, "coordinator-");
    let request = single(&operation, "request-");
    let mut command = Command::new(coordinator);
    command.args([
        "__update-coordinator",
        request.to_str().expect("utf-8 request"),
    ]);
    for (key, value) in &fixture.env {
        command.env(key, value);
    }
    let status = command.output().expect("rerun coordinator");
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("\"status\":\"reused\""),
        "rerun coordinator stdout:\n{}\nstderr:\n{}\nfixture operation:\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr),
        fixture.operation_diagnostic(),
    );
    assert_eq!(
        fs::read(&fixture.installed).expect("destination after reuse"),
        destination
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        server.chat_bodies().len(),
        7,
        "a reused request must not recover again"
    );
}

#[test]
#[ignore = "replaces an executable; run only in Windows release CI"]
fn failed_cargo_install_recovers_with_the_retained_old_binary_once() {
    const NOTICE_HEAD: &str = "update handoff could not install the new build";
    const NOTICE_TAIL: &str = "(install_failed); recovered with the previous";
    const NOTICE_END: &str = "build";

    let server = MockServer::start(update_conversation());
    let fixture = Fixture::new(true);
    let mut session = fixture.start(&server);
    session.wait_for("mock-model");
    session.send("install update\r");
    wait_until("tool-result continuation before handoff", || {
        (server.chat_bodies().len() == 5 && session.screen().contains("turn-after-update"))
            .then_some(())
    });
    let _ = session.wait_for_exit(&fixture);

    let recovery_started = Instant::now();
    loop {
        let screen = session.screen();
        if server.chat_bodies().len() == 7 && screen.contains("continued-after-update") {
            break;
        }
        assert!(
            recovery_started.elapsed() < WAIT,
            "timed out waiting for retained-binary recovery continuation; chat count: {}\nfixture Cargo log:\n{}\nfixture operation:\n{}\ncoordinator output / recovery screen:\n{screen}",
            server.chat_bodies().len(),
            fs::read_to_string(&fixture.cargo_log).unwrap_or_default(),
            fixture.operation_diagnostic(),
        );
        std::thread::sleep(POLL);
    }
    assert_eq!(
        fs::read(&fixture.installed).expect("retained installed binary"),
        fixture.old_binary,
        "a failed Cargo install must retain the old default Cargo binary"
    );
    let cargo_log = fs::read_to_string(&fixture.cargo_log).expect("read fixture Cargo log");
    assert!(
        cargo_log.contains("argv: install --path apps/hrdr --locked --force --bin hrdr")
            && cargo_log.contains("forced install failure"),
        "the fixture must force Cargo install to fail rather than fabricate success:\n{cargo_log}"
    );
    let screen = session.screen();
    let compact_screen = compact_terminal_text(&screen);
    let compact_head = compact_terminal_text(NOTICE_HEAD);
    let compact_tail = compact_terminal_text(NOTICE_TAIL);
    let compact_end = compact_terminal_text(NOTICE_END);
    assert_eq!(
        compact_screen.matches(&compact_head).count(),
        1,
        "update failure notice:\n{screen}"
    );
    let tail = compact_screen
        .find(&compact_tail)
        .expect("update failure notice tail");
    assert!(
        compact_screen[tail + compact_tail.len()..].contains(&compact_end),
        "update failure notice:\n{screen}"
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        server.chat_bodies().len(),
        7,
        "the retained old binary must run exactly one continuation without a dual recovery"
    );
}

fn update_conversation() -> Vec<Chat> {
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
            common::text_chunk("continued", "turn-after-update"),
            stop_chunk("continued"),
            "[DONE]".to_owned(),
        ]),
        Chat::Sse(vec![
            tool_start_chunk("shell", "recovery-cwd", "shell"),
            tool_args_chunk(
                "shell",
                r#"{"command":"if (Test-Path src/main.rs) { 'recovery-cwd-preserved' } else { throw 'recovery cwd changed' }"}"#,
            ),
            tool_calls_stop_chunk("shell"),
            "[DONE]".to_owned(),
        ]),
        Chat::Sse(vec![
            common::text_chunk("recovered", "continued-after-update"),
            stop_chunk("recovered"),
            "[DONE]".to_owned(),
        ]),
    ]
}

fn persisted_session_state(home: &Path) -> Option<serde_json::Value> {
    let sessions = home.join("hrdr").join("sessions");
    let cwd = fs::read_dir(sessions)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| entry.file_type().ok().is_some_and(|kind| kind.is_dir()))?;
    let snapshot = fs::read_dir(cwd.path())
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

fn compact_terminal_text(text: &str) -> String {
    text.chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '(' | ')' | ';')
        })
        .collect()
}

fn wait_until<T>(what: &str, mut observe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(value) = observe() {
            return value;
        }
        assert!(start.elapsed() < WAIT, "timed out waiting for {what}");
        std::thread::sleep(POLL);
    }
}

fn copy_tree(source: &Path, destination: &Path) {
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

fn cargo_program() -> PathBuf {
    let output = Command::new("where.exe")
        .arg("cargo.exe")
        .output()
        .expect("locate cargo.exe");
    assert!(output.status.success(), "cargo.exe unavailable");
    PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("utf-8 where output")
            .lines()
            .next()
            .expect("cargo.exe path"),
    )
}

fn git_program() -> PathBuf {
    let output = Command::new("where.exe")
        .arg("git.exe")
        .output()
        .expect("locate git.exe");
    assert!(output.status.success(), "git.exe unavailable");
    PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("utf-8 where output")
            .lines()
            .next()
            .expect("git.exe path"),
    )
}

fn git_output(args: &[&str]) -> String {
    let output = Command::new("git").args(args).output().expect("run git");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf-8 git output")
        .trim()
        .to_owned()
}

fn git_output_in(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf-8 git output")
        .trim()
        .to_owned()
}

fn run_git(args: &[&str]) {
    let output = Command::new("git").args(args).output().expect("run git");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_git_in(directory: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn single(directory: &Path, prefix: &str) -> PathBuf {
    fs::read_dir(directory)
        .expect("read operation directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix))
        })
        .unwrap_or_else(|| panic!("no {prefix} in {}", directory.display()))
}

fn trust(home: &Path, project: &Path) {
    let directory = home.join("hrdr");
    fs::create_dir_all(&directory).expect("trust directory");
    fs::write(
        directory.join("trusted-dirs"),
        format!(
            "{}\n",
            fs::canonicalize(project)
                .expect("canonical checkout")
                .display()
        ),
    )
    .expect("trust checkout");
}

fn gh_script(head: &str) -> String {
    let jobs = REQUIRED_JOBS
        .iter()
        .map(|name| format!(r#"{{"name":"{name}","status":"completed","conclusion":"success"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "@echo off\r\nif \"%~1\"==\"api\" (echo [{{\"workflow_runs\":[{{\"name\":\"CI\",\"event\":\"push\",\"head_branch\":\"main\",\"head_sha\":\"{head}\",\"created_at\":\"2026-10-11T00:00:00Z\",\"id\":1,\"run_attempt\":1,\"status\":\"completed\",\"conclusion\":\"success\"}}]}}]& exit /b 0)\r\necho {{\"jobs\":[{jobs}]}}\r\n"
    )
}

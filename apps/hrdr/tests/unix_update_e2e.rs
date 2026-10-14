#![cfg(unix)]

// Unix self-update, end to end: a real TUI in a pty asks for the update, the
// running process installs the admitted checkout into its (private) default
// Cargo home, commits the handoff, and execs the installed binary, which claims
// the checkpoint as its successor and continues the turn in the same terminal.
// Ignored by default — it runs a release `cargo install` — and run by the
// Linux and macOS smoke jobs.
extern crate hrdr_test_support;

mod common;

use common::MockServer;
use common::update::{
    ci_responses, clone_working_tree, copy_tree, persisted_session_state, update_conversation,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(180);
/// The update installs a from-scratch release build before it restarts.
const INSTALL_WAIT: Duration = Duration::from_secs(20 * 60);
const POLL: Duration = Duration::from_millis(100);

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    checkout: PathBuf,
    expected_commit: String,
    installed: PathBuf,
    env: Vec<(OsString, OsString)>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("private update fixture root");
        let home = root.path().join("home");
        let cargo_home = root.path().join("cargo-home");
        let checkout = root.path().join("checkout");
        let bin = root.path().join("bin");
        for dir in [&home, &cargo_home.join("bin"), &bin] {
            fs::create_dir_all(dir).expect("fixture directory");
        }

        // The sandbox ctor replaced HOME, so the real toolchain and caches are
        // found from the home it recorded.
        let real_home = hrdr_test_support::real_home().expect("the real home");
        let source_cargo = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| real_home.join(".cargo"));
        let rustup_home = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| real_home.join(".rustup"));
        // Cargo must not touch the developer's cache once the update has
        // started; copy only the dependency caches, never credentials or config.
        for name in ["registry", "git"] {
            let source = source_cargo.join(name);
            if source.exists() {
                copy_tree(&source, &cargo_home.join(name));
            }
        }

        let installed = cargo_home.join("bin").join("hrdr");
        fs::copy(env!("CARGO_BIN_EXE_hrdr"), &installed).expect("install test binary");

        clone_working_tree(&checkout, "unix");
        let head = git(Some(&checkout), &["rev-parse", "--verify", "HEAD^{commit}"]);
        git(Some(&checkout), &["switch", "-C", "main", &head]);
        git(
            Some(&checkout),
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/kryptic-sh/hrdr.git",
            ],
        );

        // `git ls-remote` answers with this commit as origin's main; every other
        // git command is the real one. `gh` reports a green CI run for it.
        let real_git = which("git");
        script(
            &bin.join("git"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = ls-remote ]; then printf '%s\\trefs/heads/main\\n' {head}; exit 0; fi\nexec '{}' \"$@\"\n",
                real_git.display()
            ),
        );
        let (runs, jobs) = ci_responses(&head);
        script(
            &bin.join("gh"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = api ]; then cat <<'EOF'\n{runs}\nEOF\nexit 0; fi\ncat <<'EOF'\n{jobs}\nEOF\n"
            ),
        );

        let path = std::env::join_paths([bin.clone()].into_iter().chain(std::env::split_paths(
            &std::env::var_os("PATH").expect("PATH"),
        )))
        .expect("fixture PATH");
        let runtime = root.path().join("runtime");
        fs::create_dir_all(&runtime).expect("runtime dir");
        let mut env: Vec<(OsString, OsString)> = [
            ("CARGO_HOME", cargo_home.as_os_str()),
            ("RUSTUP_HOME", rustup_home.as_os_str()),
            ("CARGO_NET_OFFLINE", "true".as_ref()),
            ("HOME", home.as_os_str()),
            ("XDG_CONFIG_HOME", home.as_os_str()),
            ("XDG_DATA_HOME", home.as_os_str()),
            ("XDG_STATE_HOME", home.as_os_str()),
            ("XDG_CACHE_HOME", home.as_os_str()),
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        ]
        .into_iter()
        .map(|(key, value)| (OsString::from(key), value.to_os_string()))
        .collect();
        env.push(("PATH".into(), path));

        Self {
            _root: root,
            home,
            checkout,
            expected_commit: head,
            installed,
            env,
        }
    }

    fn start(&self, server: &MockServer) -> Session {
        common::write_config(&self.home, &server.base_url());
        // Trust is an exact match on each canonical directory, so the
        // `apps/hrdr` the test moves into is trusted too.
        let trusted = self.home.join("hrdr");
        fs::create_dir_all(&trusted).expect("trust store dir");
        let entries: String = [
            self.checkout.clone(),
            self.checkout.join("apps").join("hrdr"),
        ]
        .iter()
        .map(|dir| {
            format!(
                "{}\n",
                fs::canonicalize(dir).expect("canonical dir").display()
            )
        })
        .collect();
        fs::write(trusted.join("trusted-dirs"), entries).expect("trust the checkout");
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let mut command = CommandBuilder::new(&self.installed);
        command.args(["--no-auto-resume", "--no-bell"]);
        command.cwd(&self.checkout);
        command.env("TERM", "xterm-256color");
        // A target directory inherited from the developer's or CI's cargo would
        // be the one the running `cargo test` holds locked; the update's own
        // install must build in a fresh one.
        for key in [
            "HRDR_MODEL",
            "HRDR_API_KEY",
            "RUST_LOG",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_TARGET_DIR",
        ] {
            command.env_remove(key);
        }
        // The install builds the clone cold, in its own `target`. A local rerun
        // may point it at a persistent directory to skip the cold build; CI
        // leaves this unset.
        if let Some(dir) = std::env::var_os("HRDR_E2E_INSTALL_TARGET_DIR") {
            command.env("CARGO_TARGET_DIR", dir);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let child = pty
            .slave
            .spawn_command(command)
            .expect("spawn installed hrdr");
        drop(pty.slave);
        let reader = pty.master.try_clone_reader().expect("pty reader");
        let writer: Writer = Arc::new(Mutex::new(pty.master.take_writer().expect("pty writer")));
        let screen = common::drain_pty(reader, Arc::clone(&writer));
        Session {
            child,
            _master: pty.master,
            writer,
            screen,
        }
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
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(text.as_bytes()).expect("write pty");
        writer.flush().expect("flush pty");
    }

    fn screen(&self) -> String {
        common::visible(&self.screen.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Poll until `done` holds, failing early if the process exits.
    fn wait_until(&mut self, what: &str, budget: Duration, mut done: impl FnMut(&str) -> bool) {
        let start = Instant::now();
        loop {
            let screen = self.screen();
            if done(&screen) {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll hrdr") {
                panic!("hrdr exited ({status:?}) before {what}:\n{screen}");
            }
            assert!(
                start.elapsed() < budget,
                "timed out waiting for {what}:\n{screen}"
            );
            std::thread::sleep(POLL);
        }
    }
}

#[test]
#[ignore = "runs a release cargo install; run by the unix smoke jobs"]
fn update_installs_and_execs_a_successor_that_continues_the_turn() {
    let server = MockServer::start(update_conversation());
    let fixture = Fixture::new();
    let mut session = fixture.start(&server);
    let pid = session.child.process_id();
    session.wait_until("the first frame", WAIT, |screen| {
        screen.contains("mock-model")
    });
    session.send("/cwd apps/hrdr\r");
    // The success line; a refusal names the same path.
    session.wait_until("the cwd change", WAIT, |screen| screen.contains("cwd → "));
    session.send("install update\r");
    session.wait_until("the update turn", WAIT, |screen| {
        server.chat_bodies().len() == 5 && screen.contains("turn-after-update")
    });

    // Install, commit, exec, recover, continue — all in the same terminal. A
    // failed install is reported in a transient notice and then waits for the
    // user, so the raw terminal stream is watched for it: it ends the wait at
    // once rather than when the budget runs out.
    let raw = Arc::clone(&session.screen);
    session.wait_until("the successor's continuation", INSTALL_WAIT, |screen| {
        let raw = raw.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = raw
            .find("update preparation failed")
            .or_else(|| raw.find("update handoff failed"))
        {
            let report: String = raw[at..].chars().take(1500).collect();
            panic!("the update failed: {report:?}");
        }
        server.chat_bodies().len() == 7 && screen.contains("continued-after-update")
    });
    assert_eq!(
        session.child.process_id(),
        pid,
        "exec keeps the process: the successor is the same terminal child"
    );
    let build_info = Command::new(&fixture.installed)
        .arg("--build-info")
        .output()
        .expect("installed build info");
    assert_eq!(
        String::from_utf8(build_info.stdout).expect("utf-8 build info"),
        format!(
            "version: {}\ncommit: {}\ntarget: {}\nsource: clean\n",
            env!("CARGO_PKG_VERSION"),
            fixture.expected_commit,
            env!("HRDR_BUILD_TARGET"),
        ),
        "the default Cargo binary is the admitted checkout"
    );
    assert!(
        server
            .chat_bodies()
            .last()
            .is_some_and(|body| body.to_string().contains("recovery-cwd-preserved")),
        "the successor's shell ran in the session's own directory"
    );
    let state = persisted_session_state(&fixture.home).expect("the session was saved");
    assert!(
        state["todos"].as_array().is_some_and(|todos| todos
            .iter()
            .any(|todo| todo["content"] == "preserve update state")),
        "the successor kept the session's TODOs: {state}"
    );

    session.send("\x11");
    let start = Instant::now();
    while session.child.try_wait().expect("poll hrdr").is_none() {
        assert!(start.elapsed() < WAIT, "the successor did not quit");
        std::thread::sleep(POLL);
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

fn which(program: &str) -> PathBuf {
    let output = Command::new("sh")
        .args(["-c", &format!("command -v {program}")])
        .output()
        .expect("locate program");
    assert!(output.status.success(), "{program} unavailable");
    PathBuf::from(String::from_utf8(output.stdout).expect("utf-8 path").trim())
}

fn script(path: &Path, body: &str) {
    fs::write(path, body).expect("write fixture script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("make script executable");
}

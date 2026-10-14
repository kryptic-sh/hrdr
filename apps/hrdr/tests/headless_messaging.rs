//! `hrdr run` and session messaging, across real processes: a headless run is
//! discoverable while its prompt's turn runs, takes a peer's message into that
//! turn's queue, still exits once the accepted work drains, and stays hidden
//! when messaging is off.
//!
//! The peer is a messaging runtime in this test process. Both sides use the
//! suite's private discovery directory: `HRDR_IPC_DIR`, set by the sandbox ctor
//! and inherited by the child.

extern crate hrdr_test_support;

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{Chat, MockServer, stop_chunk, text_chunk, write_config};
use hrdr_agent::messaging::{Messaging, SendStatus};
use hrdr_tools::local_ipc::{SessionDescriptor, UserDirectory};

const WAIT: Duration = Duration::from_secs(60);

fn text(id: &str, reply: &str) -> Vec<String> {
    vec![text_chunk(id, reply), stop_chunk(id), "[DONE]".to_string()]
}

struct Run {
    child: Child,
    _home: tempfile::TempDir,
    _project: tempfile::TempDir,
}

fn spawn_run(server: &MockServer, env: &[(&str, &str)]) -> Run {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), &server.base_url());
    let project = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hrdr"));
    command
        .args(["run", "summarize the repository"])
        .current_dir(project.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in [
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        command.env(key, home.path());
    }
    for key in ["HRDR_MODEL", "HRDR_API_KEY", "RUST_LOG"] {
        command.env_remove(key);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    Run {
        child: command.spawn().expect("spawn hrdr run"),
        _home: home,
        _project: project,
    }
}

async fn peer() -> Messaging {
    Messaging::start(
        UserDirectory::open().unwrap(),
        format!("headless-peer-{}", std::process::id()),
        "peer".into(),
        "/peer".into(),
    )
    .await
    .unwrap()
}

async fn find_run(peer: &Messaging, pid: u32) -> Option<SessionDescriptor> {
    peer.discover()
        .await
        .unwrap()
        .into_iter()
        .find(|session| session.session_id == format!("run-{pid}"))
}

async fn wait_for_request(server: &MockServer) {
    let start = Instant::now();
    while server.chat_bodies().is_empty() {
        assert!(start.elapsed() < WAIT, "the run never reached the model");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn finish(run: Run) -> std::process::Output {
    tokio::time::timeout(
        WAIT,
        tokio::task::spawn_blocking(move || run.child.wait_with_output()),
    )
    .await
    .expect("the run ends once its accepted work drains")
    .unwrap()
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_message_joins_a_running_headless_turn_and_the_run_still_ends() {
    let (release, released) = std::sync::mpsc::channel();
    let server = MockServer::start(vec![
        Chat::Gated(released, text("first", "summary written")),
        Chat::Sse(text("second", "peer request handled")),
    ]);
    let run = spawn_run(&server, &[]);
    let pid = run.child.id();
    wait_for_request(&server).await;

    let peer = peer().await;
    let target = find_run(&peer, pid)
        .await
        .expect("the running headless turn is discoverable");
    assert_eq!(target.session_name, "summarize the repository");
    let mut run = run;
    let sent = peer.send(&target, "also list the open TODOs".into()).await;
    let sent = match sent {
        Ok(status) => status,
        Err(error) => {
            // Say what the run itself reported; a bare transport error does not.
            let _ = run.child.kill();
            let output = run
                .child
                .wait_with_output()
                .expect("collect the run's output");
            panic!(
                "sending to the headless run failed: {error}\nrun stderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    };
    assert_eq!(sent, SendStatus::Accepted);
    release.send(()).unwrap();

    let output = finish(run).await;
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("summary written"), "{stdout}");
    assert!(stdout.contains("peer request handled"), "{stdout}");
    let bodies = server.chat_bodies();
    assert_eq!(bodies.len(), 2, "the prompt's turn, then one for the peer");
    let second = bodies[1].to_string();
    assert!(second.contains("also list the open TODOs"), "{second}");
    assert!(second.contains("not human input"), "{second}");
    assert!(
        second.contains("headless-peer-"),
        "the sender is named: {second}"
    );
    peer.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_headless_run_with_messaging_off_is_not_discoverable() {
    let (release, released) = std::sync::mpsc::channel();
    let server = MockServer::start(vec![Chat::Gated(released, text("only", "done"))]);
    let run = spawn_run(&server, &[("HRDR_SESSION_MESSAGING", "0")]);
    let pid = run.child.id();
    // Registration happens before the first request, so by now it would exist.
    wait_for_request(&server).await;
    let peer = peer().await;
    assert!(find_run(&peer, pid).await.is_none());
    release.send(()).unwrap();
    assert!(finish(run).await.status.success());
    peer.close().await;
}

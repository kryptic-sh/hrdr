use super::*;

pub(super) struct WorkerExit {
    entered: Notify,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}
pub(super) struct WorkerExitGuard(pub Option<Arc<WorkerExit>>);
impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        if let Some(exit) = &self.0 {
            exit.entered.notify_one();
            exit.release.lock().unwrap().recv_timeout(DEADLINE).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_waits_for_worker_destruction() {
    let root = root();
    let target = runtime(root.path(), "target", "/target").await;
    let (release, released) = std::sync::mpsc::channel();
    let exit = Arc::new(WorkerExit {
        entered: Notify::new(),
        release: Mutex::new(released),
    });
    *target.shared.worker_exit.lock().unwrap() = Some(exit.clone());
    let mut peer = Connection::connect(&target.directory, &target.descriptor().endpoint, DEADLINE)
        .await
        .unwrap();
    let _: Hello = protocol::read(&mut peer).await.unwrap();
    let mut closing = tokio::spawn(async move { target.close().await });
    timeout(DEADLINE, exit.entered.notified()).await.unwrap();
    let premature = timeout(Duration::from_millis(50), &mut closing).await;
    release.send(()).unwrap();
    assert!(
        premature.is_err(),
        "close returned before worker destruction"
    );
    assert_eq!(closing.await.unwrap(), 0);
}

#[tokio::test]
async fn an_incompatible_session_does_not_hide_the_others() {
    use hrdr_tools::local_ipc::{Listener, Registration};

    let root = root();
    let a = runtime(root.path(), "a", "/one").await;
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let mut listener = Listener::bind(&directory).unwrap();
    let descriptor = SessionDescriptor {
        endpoint: listener.endpoint().clone(),
        session_id: "older".into(),
        session_name: "older".into(),
        working_directory: "/older".into(),
    };
    let _registration = Registration::publish(&listener, descriptor).unwrap();
    let older = tokio::spawn(async move {
        let mut connection = listener.accept(DEADLINE).await.unwrap();
        connection
            .write_frame(br#"{"version":99}"#, DEADLINE)
            .await
            .unwrap();
        connection
    });

    assert_eq!(a.discover().await.unwrap(), vec![a.descriptor()]);
    drop(older.await.unwrap());
}

fn root() -> tempfile::TempDir {
    #[cfg(unix)]
    let root = tempfile::tempdir_in("/tmp");
    #[cfg(windows)]
    let root = tempfile::tempdir();
    root.unwrap()
}
async fn runtime(root: &std::path::Path, id: &str, cwd: &str) -> Messaging {
    Messaging::start(
        UserDirectory::open_in(root).unwrap(),
        id.into(),
        "duplicate".into(),
        cwd.into(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn close_reaps_finished_listener_once() {
    let root = root();
    let target = runtime(root.path(), "target", "/target").await;
    target
        .shutdown
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .send(())
        .unwrap();
    let task = target.task.lock().unwrap().as_ref().unwrap().abort_handle();
    timeout(DEADLINE, async {
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(target.close().await, 0);
    assert!(target.task.lock().unwrap().is_none());
    assert_eq!(target.close().await, 0);
}

#[tokio::test]
async fn close_joins_native_handshake_workers() {
    let root = root();
    let target = runtime(root.path(), "target", "/target").await;
    let mut peers = Vec::new();
    for _ in 0..MAX_WORKERS {
        let mut peer =
            Connection::connect(&target.directory, &target.descriptor().endpoint, DEADLINE)
                .await
                .unwrap();
        let _: Hello = protocol::read(&mut peer).await.unwrap();
        peers.push(peer);
    }
    assert_eq!(Arc::strong_count(&target.shared), MAX_WORKERS + 2);
    assert_eq!(target.close().await, 0);
    assert_eq!(
        Arc::strong_count(&target.shared),
        1,
        "workers outlived close"
    );
    for mut peer in peers {
        assert!(protocol::read::<Reply>(&mut peer).await.is_err());
    }
    assert_eq!(target.close().await, 0);
}

#[tokio::test]
async fn close_racing_delivery_preserves_every_accepted_message() {
    let root = root();
    let sender = runtime(root.path(), "sender", "/sender").await;
    for _ in 0..16 {
        let target = runtime(root.path(), "target", "/target").await;
        let descriptor = target.descriptor();
        let delivery = sender.send(&descriptor, "racing".into());
        let close = async {
            target.wait().await;
            target.close().await
        };
        let (status, pending) = tokio::join!(delivery, close);
        assert_eq!(pending, 1);
        if let Ok(status) = status {
            assert_eq!(status, SendStatus::Accepted);
        }
        assert_eq!(target.dequeue().unwrap().sent, "racing");
        assert!(target.dequeue().is_none());
        assert_eq!(target.close().await, 0);
    }
}

#[tokio::test]
async fn native_sessions_preserve_attribution_and_idle_wakeup() {
    let root = root();
    let a = runtime(root.path(), "a", "/one").await;
    let b = runtime(root.path(), "b", "/two").await;
    let live = a.discover().await.unwrap();
    assert_eq!(live.len(), 2);
    assert!(live.contains(&a.descriptor()));
    assert!(live.contains(&b.descriptor()));
    let target = b.descriptor();
    let ((), status) = tokio::join!(b.wait(), a.send(&target, "literal /command @file".into()));
    assert_eq!(status.unwrap(), SendStatus::Accepted);
    let message = b.dequeue().unwrap();
    assert_eq!(message.sent, "literal /command @file");
    assert_eq!(
        message.peer,
        Some(PeerIdentity {
            session_id: "a".into(),
            session_name: "duplicate".into(),
            cwd: "/one".into(),
            agent: "main".into()
        })
    );
    assert!(b.dequeue().is_none());
    assert_eq!(
        a.send(&b.descriptor(), "before poll".into()).await.unwrap(),
        SendStatus::Accepted
    );
    timeout(DEADLINE, b.wait()).await.unwrap();
    assert_eq!(b.dequeue().unwrap().sent, "before poll");
}

#[tokio::test]
async fn native_inbox_count_bytes_and_utf8_limits() {
    let root = root();
    let a = runtime(root.path(), "a", "/one").await;
    let b = runtime(root.path(), "b", "/two").await;
    let target = b.descriptor();
    for _ in 0..MAX_INBOX_MESSAGES {
        assert_eq!(
            a.send(&target, "x".into()).await.unwrap(),
            SendStatus::Accepted
        );
    }
    assert_eq!(
        a.send(&target, "overflow".into()).await.unwrap(),
        SendStatus::QueueFull
    );
    for _ in 0..MAX_INBOX_MESSAGES {
        assert_eq!(b.dequeue().unwrap().sent, "x");
    }
    let text = "é".repeat(MAX_MESSAGE_BYTES / 2);
    let metadata_bytes = "a".len() + "duplicate".len() + "/one".len() + "main".len();
    let full_messages = MAX_INBOX_BYTES / (MAX_MESSAGE_BYTES + metadata_bytes);
    for _ in 0..full_messages {
        assert_eq!(
            a.send(&target, text.clone()).await.unwrap(),
            SendStatus::Accepted
        );
    }
    let remaining = MAX_INBOX_BYTES - full_messages * (MAX_MESSAGE_BYTES + metadata_bytes);
    assert_eq!(
        a.send(&target, "r".repeat(remaining - metadata_bytes))
            .await
            .unwrap(),
        SendStatus::Accepted
    );
    assert_eq!(
        a.send(&target, "x".into()).await.unwrap(),
        SendStatus::QueueFull
    );
    assert_eq!(b.shared.state.lock().unwrap().bytes, MAX_INBOX_BYTES);
    assert_eq!(b.dequeue().unwrap().sent, text);
    assert_eq!(
        a.send(&target, text.clone()).await.unwrap(),
        SendStatus::Accepted
    );
    assert_eq!(
        a.send(&target, format!("{text}x"))
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn native_refresh_close_and_new_generation() {
    let root = root();
    let a = runtime(root.path(), "a", "/one").await;
    let b = runtime(root.path(), "b", "/two").await;
    let old = b.descriptor();
    b.refresh("renamed".into(), "/three".into()).await.unwrap();
    assert_eq!(b.descriptor().endpoint, old.endpoint);
    assert!(a.discover().await.unwrap().contains(&b.descriptor()));
    assert_eq!(
        b.send(&a.descriptor(), "refreshed".into()).await.unwrap(),
        SendStatus::Accepted
    );
    let peer = a.dequeue().unwrap().peer.unwrap();
    assert_eq!(peer.session_name, "renamed");
    assert_eq!(peer.cwd, "/three");
    assert_eq!(
        a.send(&old, "pending".into()).await.unwrap(),
        SendStatus::Accepted
    );
    assert_eq!(b.close().await, 1);
    assert_eq!(b.dequeue().unwrap().sent, "pending");
    assert!(b.dequeue().is_none());
    assert_eq!(b.close().await, 0);
    assert_eq!(
        a.send(&old, "closed".into()).await.unwrap(),
        SendStatus::Unavailable
    );
    assert_eq!(
        b.send(&a.descriptor(), "closed sender".into())
            .await
            .unwrap(),
        SendStatus::Unavailable
    );
    assert_eq!(a.discover().await.unwrap(), vec![a.descriptor()]);
    let replacement = runtime(root.path(), "b", "/four").await;
    assert_ne!(replacement.descriptor().endpoint, old.endpoint);
    let mut wrong_session = replacement.descriptor();
    wrong_session.session_id = "old session".into();
    assert_eq!(
        a.send(&wrong_session, "wrong generation".into())
            .await
            .unwrap(),
        SendStatus::SessionChanged
    );
    assert!(replacement.dequeue().is_none());
}

async fn raw(target: &Messaging, request: serde_json::Value) -> io::Result<Reply> {
    let mut connection =
        Connection::connect(&target.directory, &target.descriptor().endpoint, DEADLINE).await?;
    let hello: Hello = protocol::read(&mut connection).await?;
    assert_eq!(hello.descriptor, target.descriptor());
    protocol::write(&mut connection, &request).await?;
    protocol::read(&mut connection).await
}

#[tokio::test]
async fn native_rejects_forged_malformed_and_changed_receiver() {
    let root = root();
    let a = runtime(root.path(), "a", "/one").await;
    let b = runtime(root.path(), "b", "/two").await;
    let request = Request {
        version: VERSION,
        receiver: SessionRef::from(&b.descriptor()),
        payload: Payload::Message {
            sender: SessionRef::from(&a.descriptor()),
            text: "forged".into(),
        },
    };
    let good = serde_json::to_value(request).unwrap();
    let mut unknown = good.clone();
    unknown["payload"]["sender"]["session_name"] = "impostor".into();
    assert!(raw(&b, unknown).await.is_err());
    let mut wrong_version = good.clone();
    wrong_version["version"] = (VERSION + 1).into();
    assert!(raw(&b, wrong_version).await.is_err());
    let mut forged = good.clone();
    forged["payload"]["sender"]["session_id"] = "not registered".into();
    assert!(raw(&b, forged).await.is_err());
    let mut oversized = good.clone();
    oversized["payload"]["text"] = "x".repeat(MAX_MESSAGE_BYTES + 1).into();
    assert!(raw(&b, oversized).await.is_err());
    let mut changed = good;
    changed["receiver"]["session_id"] = "replaced".into();
    assert_eq!(
        raw(&b, changed).await.unwrap().status,
        SendStatus::SessionChanged
    );
    assert!(b.dequeue().is_none());
}

#[tokio::test]
async fn native_drop_releases_lease_and_excludes_stale() {
    let root = root();
    let a = runtime(root.path(), "a", "/one").await;
    let b = runtime(root.path(), "b", "/two").await;
    let task = b.task.lock().unwrap().as_ref().unwrap().abort_handle();
    drop(b);
    timeout(DEADLINE, async {
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(a.discover().await.unwrap(), vec![a.descriptor()]);
}

#[tokio::test]
async fn runtime_child() {
    let Some(root) = std::env::var_os("HRDR_MESSAGING_CHILD_ROOT") else {
        return;
    };
    let child = runtime(std::path::Path::new(&root), "child", "/child").await;
    let target: SessionDescriptor =
        serde_json::from_str(&std::env::var("HRDR_MESSAGING_CHILD_TARGET").unwrap()).unwrap();
    assert_ne!(target.endpoint.pid(), std::process::id());
    assert_eq!(
        child
            .send(&target, "from child process".into())
            .await
            .unwrap(),
        SendStatus::Accepted
    );
    let forged = serde_json::json!({"version": VERSION, "receiver": SessionRef::from(&target), "payload": {"kind": "message", "sender": SessionRef::from(&target), "text": "forged parent"}});
    let mut connection = Connection::connect(&child.directory, &target.endpoint, DEADLINE)
        .await
        .unwrap();
    let _: Hello = protocol::read(&mut connection).await.unwrap();
    protocol::write(&mut connection, &forged).await.unwrap();
    assert!(protocol::read::<Reply>(&mut connection).await.is_err());
}

#[tokio::test]
async fn native_cross_process_runtime_delivery_and_pid_rejection() {
    let root = root();
    let parent = runtime(root.path(), "parent", "/parent").await;
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "messaging::tests::runtime_child", "--nocapture"])
        .env("HRDR_MESSAGING_CHILD_ROOT", root.path())
        .env(
            "HRDR_MESSAGING_CHILD_TARGET",
            serde_json::to_string(&parent.descriptor()).unwrap(),
        )
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    assert!(
        timeout(Duration::from_secs(15), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let message = parent.dequeue().unwrap();
    assert_eq!(message.sent, "from child process");
    assert_eq!(
        message.peer,
        Some(PeerIdentity {
            session_id: "child".into(),
            session_name: "duplicate".into(),
            cwd: "/child".into(),
            agent: "main".into()
        })
    );
    assert!(parent.dequeue().is_none());
}

/// A busy discovery directory slows a message down without dropping it: the
/// receiver checks the sender against the directory, and while another process
/// holds the directory's lock past the wire deadline, both ends keep waiting
/// instead of hanging up mid-exchange.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_waits_out_a_directory_held_past_the_wire_deadline() {
    let root = root();
    let a = runtime(root.path(), "a", "/one").await;
    let b = runtime(root.path(), "b", "/two").await;
    let lock_path = find_file(root.path(), ".mutation.lock").expect("the directory's lock file");
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    held.lock().unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(DEADLINE + Duration::from_secs(1));
        held.unlock().unwrap();
    });

    assert_eq!(
        a.send(&b.descriptor(), "through a busy directory".into())
            .await
            .unwrap(),
        SendStatus::Accepted
    );
    release.join().unwrap();
    assert_eq!(b.dequeue().unwrap().sent, "through a busy directory");
    a.close().await;
    b.close().await;
}

fn find_file(root: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|file| file == name) {
            return Some(path);
        }
        if path.is_dir()
            && let Some(found) = find_file(&path, name)
        {
            return Some(found);
        }
    }
    None
}

/// A registered session that answers a probe without admitting messages is
/// left out of the live list but still reported, with the answer it gave —
/// what lets a listing say why a session the user can see is missing.
#[tokio::test]
async fn discovery_reports_a_session_that_did_not_accept_and_why() {
    let root = root();
    let a = runtime(root.path(), "a", "/one").await;
    let b = runtime(root.path(), "b", "/two").await;
    b.shared.state.lock().unwrap().open = false;

    let found = a.discover_detailed().await.unwrap();
    assert_eq!(found.live, vec![a.descriptor()]);
    assert_eq!(found.unreachable.len(), 1, "{:?}", found.unreachable);
    let (session, reason) = &found.unreachable[0];
    assert_eq!(session, &b.descriptor());
    assert_eq!(reason, "answered Unavailable");
    assert_eq!(a.discover().await.unwrap(), vec![a.descriptor()]);
}

/// A session answering `Unavailable` once — the shape a connect that failed
/// transiently takes — gets the same single retry a failed probe does, so a
/// live session is not dropped from the listing on one miss.
#[tokio::test]
async fn discovery_retries_a_session_that_was_unavailable_once() {
    let root = root();
    let a = runtime(root.path(), "a", "/one").await;
    let b = runtime(root.path(), "b", "/two").await;
    b.shared.state.lock().unwrap().open = false;
    let shared = Arc::clone(&b.shared);
    let reopen = tokio::spawn(async move {
        tokio::time::sleep(PROBE_RETRY_DELAY / 4).await;
        shared.state.lock().unwrap().open = true;
    });

    let found = a.discover_detailed().await.unwrap();
    reopen.await.unwrap();
    assert!(found.live.contains(&b.descriptor()), "{found:?}");
    assert!(found.unreachable.is_empty(), "{found:?}");
}

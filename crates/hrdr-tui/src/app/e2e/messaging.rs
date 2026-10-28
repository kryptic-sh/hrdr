//! Session messaging through a real TUI app: discovery, idle wake, busy-agent
//! queueing, the disable flag and session rotation. The peer is a second runtime
//! in the same process, registered in the suite's private discovery directory
//! (`HRDR_IPC_DIR`, set by `hrdr-test-support`).

use super::super::messaging_tools::{MessageSessionTool, SessionsTool};
use super::*;
use hrdr_agent::messaging::{Messaging, SendStatus};
use hrdr_tools::Tool;
use hrdr_tools::local_ipc::{SessionDescriptor, UserDirectory};

/// Start the app's messaging for its (freshly reserved) session.
async fn started(h: &mut Harness, name: &str) -> String {
    h.app.reserve_session_id(name);
    h.app.await_saves().await;
    h.app.sync_messaging();
    h.wait_for("messaging start", |msg| {
        matches!(msg, TurnMsg::MessagingStarted(..))
    })
    .await;
    assert!(
        h.app.messaging.is_active(),
        "messaging started: {:?}",
        h.app.toasts.last_body()
    );
    h.app.state().id.clone().expect("a reserved session id")
}

async fn peer(name: &str) -> Messaging {
    Messaging::start(
        UserDirectory::open().unwrap(),
        format!("peer-{name}-{}", std::process::id()),
        name.to_string(),
        "/peer/cwd".to_string(),
    )
    .await
    .unwrap()
}

async fn find(peer: &Messaging, session_id: &str) -> Option<SessionDescriptor> {
    peer.discover()
        .await
        .unwrap()
        .into_iter()
        .find(|descriptor| descriptor.session_id == session_id)
}

fn peer_texts(h: &Harness) -> Vec<String> {
    h.app
        .agent
        .try_lock()
        .unwrap()
        .messages_owned()
        .iter()
        .filter_map(|message| message.content.clone())
        .filter(|text| text.contains("[Peer message"))
        .collect()
}

#[tokio::test]
async fn a_peer_message_wakes_an_idle_session_with_its_provenance() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![MockReply::Text("noted".into())]).await;
    let id = started(&mut h, "messaging target").await;

    let sender = peer("sender").await;
    let target = find(&sender, &id)
        .await
        .expect("the session is discoverable");
    assert_eq!(
        target.session_name,
        super::super::messaging::published_name(&h.app.state().name, &id)
    );
    assert_eq!(
        target.working_directory,
        h.app.dir_cwd.display().to_string()
    );
    assert_eq!(
        sender.send(&target, "build is green".into()).await.unwrap(),
        SendStatus::Accepted
    );

    h.wait_for("peer arrival", |msg| matches!(msg, TurnMsg::PeerReady))
        .await;
    assert!(!h.app.running());
    h.app.maybe_deliver_peer();
    assert!(h.app.running(), "the arrival wakes the idle agent");
    h.pump().await;

    let texts = peer_texts(&h);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains("not human input"), "{}", texts[0]);
    assert!(texts[0].contains("build is green"), "{}", texts[0]);
    assert!(
        texts[0].contains("\"session_name\":\"sender\""),
        "{}",
        texts[0]
    );
    sender.close().await;
}

/// Start the app's messaging before the session has an id, as opening hrdr does.
async fn started_provisional(h: &mut Harness) -> String {
    assert!(h.app.state().id.is_none());
    h.app.sync_messaging();
    h.wait_for("messaging start", |msg| {
        matches!(msg, TurnMsg::MessagingStarted(..))
    })
    .await;
    assert!(
        h.app.messaging.is_active(),
        "messaging started: {:?}",
        h.app.toasts.last_body()
    );
    super::super::messaging::provisional_session_id()
}

/// A session nobody has typed into is still a running session: it is
/// published from the start, a peer can message it, and the message wakes it —
/// a turn that gives the session its own id, as a typed first message would.
#[tokio::test]
async fn a_new_session_can_be_messaged_before_its_first_message() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![MockReply::Text("here".into())]).await;
    let provisional = started_provisional(&mut h).await;

    let sender = peer("early-sender").await;
    let target = find(&sender, &provisional)
        .await
        .expect("the new session is discoverable");
    assert_eq!(
        sender.send(&target, "are you there?".into()).await.unwrap(),
        SendStatus::Accepted
    );
    h.wait_for("peer arrival", |msg| matches!(msg, TurnMsg::PeerReady))
        .await;
    h.app.maybe_deliver_peer();
    assert!(h.app.running(), "the arrival wakes the new session");
    assert!(
        h.app.state().id.is_some(),
        "the woken turn claimed an id before it ran"
    );
    h.pump().await;

    let texts = peer_texts(&h);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains("are you there?"), "{}", texts[0]);
    sender.close().await;
}

/// A start that fails is retried, not given up: an unpublished session cannot
/// be found, so one transient failure must not leave it unlisted. The failure
/// is announced once, and the retry wakes an idle loop on its own.
#[tokio::test]
async fn a_failed_start_is_retried_and_announced_once() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    h.app.messaging.fail_next_starts = 2;
    h.app
        .messaging
        .set_first_retry_delay(std::time::Duration::from_millis(50));
    // The bus folds a repeated toast into one entry and counts the repeats, so
    // the deliveries are the sum of those counts.
    let failure_notices = |h: &Harness| {
        h.app
            .toasts
            .history()
            .filter(|toast| toast.body.contains("session messaging unavailable"))
            .map(|toast| toast.count)
            .sum::<u32>()
    };

    for attempt in 1..=2 {
        h.app.sync_messaging();
        h.wait_for("a failed start", |msg| {
            matches!(msg, TurnMsg::MessagingStarted(_, _, Err(_)))
        })
        .await;
        assert!(!h.app.messaging.is_active(), "attempt {attempt} failed");
        h.wait_for("the retry wake", |msg| {
            matches!(msg, TurnMsg::MessagingRetry)
        })
        .await;
    }
    assert_eq!(failure_notices(&h), 1, "one notice, not one per attempt");

    let provisional = started_provisional(&mut h).await;
    let sender = peer("retry-sender").await;
    assert!(
        find(&sender, &provisional).await.is_some(),
        "the retried start published the session"
    );
    sender.close().await;
}

/// The first message names the session, and its registration is renamed in
/// place: the same endpoint, now under the session's id, with what a peer sent
/// before the rename still waiting in the inbox.
#[tokio::test]
async fn a_new_session_keeps_its_registration_and_inbox_when_it_gets_an_id() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    let provisional = started_provisional(&mut h).await;
    let sender = peer("rename-sender").await;
    let before = find(&sender, &provisional).await.unwrap();
    assert_eq!(
        sender.send(&before, "early".into()).await.unwrap(),
        SendStatus::Accepted
    );
    assert_eq!(h.app.messaging.pending(), 1);

    h.app.reserve_session_id("named by its first message");
    h.app.await_saves().await;
    let id = h.app.state().id.clone().unwrap();
    h.app.sync_messaging();
    h.wait_for("the rename", |msg| {
        matches!(msg, TurnMsg::MessagingRefreshed(..))
    })
    .await;

    let after = find(&sender, &id).await.expect("listed under its own id");
    assert_eq!(after.endpoint, before.endpoint, "renamed, not rotated");
    assert!(
        find(&sender, &provisional).await.is_none(),
        "the provisional id is gone"
    );
    assert_eq!(h.app.messaging.pending(), 1, "the inbox carried over");
    sender.close().await;
}

#[tokio::test]
async fn a_busy_session_takes_one_peer_message_at_a_time() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    let id = started(&mut h, "busy target").await;
    let sender = peer("busy-sender").await;
    let target = find(&sender, &id).await.unwrap();
    for text in ["first", "second"] {
        assert_eq!(
            sender.send(&target, text.into()).await.unwrap(),
            SendStatus::Accepted
        );
    }
    assert_eq!(h.app.messaging.pending(), 2);

    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.app.maybe_deliver_peer();
    h.app.maybe_deliver_peer();
    assert_eq!(
        h.app.registry.pending(hrdr_agent::MAIN_KEY).len(),
        1,
        "only one peer message waits behind the running turn"
    );
    assert_eq!(
        h.app.messaging.pending(),
        1,
        "the rest stays in the bounded inbox"
    );
    assert!(
        !h.app.handoff_quiescent(),
        "undelivered peer messages block an update handoff"
    );
    sender.close().await;
}

#[tokio::test]
async fn exit_counts_the_queued_peer_message_and_the_inbox_as_undelivered() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    let id = started(&mut h, "exiting target").await;
    let sender = peer("exit-sender").await;
    let target = find(&sender, &id).await.unwrap();
    for text in ["queued", "in the inbox"] {
        assert_eq!(
            sender.send(&target, text.into()).await.unwrap(),
            SendStatus::Accepted
        );
    }
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.app.maybe_deliver_peer();
    assert_eq!(h.app.registry.pending_peers(hrdr_agent::MAIN_KEY), 1);
    assert_eq!(h.app.messaging.pending(), 1);

    // The quit cancels the running turn, so neither message reaches the agent:
    // the queued one is discarded with the turn, and nothing more is taken from
    // the inbox on the way out.
    h.app.request_quit();
    h.app.maybe_deliver_peer();
    assert_eq!(h.app.messaging.pending(), 1, "the inbox is left alone");
    h.app.shutdown_messaging().await;
    assert_eq!(h.app.messaging.undelivered_on_exit, 2);
    assert!(
        find(&sender, &id).await.is_none(),
        "the exiting session leaves discovery"
    );
    sender.close().await;
}

#[tokio::test]
async fn disabled_messaging_never_registers_the_session() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    h.app.messaging = super::super::messaging::MessagingState::new(false);
    h.app.reserve_session_id("hidden session");
    h.app.await_saves().await;
    h.app.sync_messaging();
    assert!(!h.app.messaging.is_active());
    assert!(!h.app.messaging.is_starting());

    let id = h.app.state().id.clone().unwrap();
    let sender = peer("disabled-sender").await;
    assert!(find(&sender, &id).await.is_none());
    sender.close().await;
}

#[tokio::test]
async fn a_replaced_session_stops_publishing_the_old_one() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    let id = started(&mut h, "replaced session").await;
    let sender = peer("rotation-sender").await;
    let old = find(&sender, &id).await.unwrap();

    h.type_str("/new");
    h.press(KeyCode::Enter);
    assert!(h.app.state().id.is_none());
    h.app.sync_messaging();
    assert!(
        !h.app.messaging.is_active(),
        "the old registration is closed"
    );
    let unregistered = async {
        while find(&sender, &id).await.is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), unregistered)
        .await
        .expect("the old session leaves discovery");
    assert_eq!(
        sender.send(&old, "late".into()).await.unwrap(),
        SendStatus::Unavailable,
        "the old session no longer accepts messages"
    );
    sender.close().await;
}

async fn call(tool: &dyn Tool, args: serde_json::Value) -> anyhow::Result<String> {
    tool.execute(args, &hrdr_tools::ToolContext::new(".")).await
}

#[tokio::test]
async fn the_model_lists_other_sessions_and_messages_one_as_this_session() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    let id = started(&mut h, "tool sender").await;
    let sessions = SessionsTool {
        view: h.app.messaging.view(),
    };
    let send = MessageSessionTool {
        view: h.app.messaging.view(),
    };
    let recipient = peer("recipient").await;
    let recipient_id = recipient.descriptor().session_id;

    let listed = call(&sessions, serde_json::json!({})).await.unwrap();
    assert!(listed.starts_with("<untrusted-content"), "{listed}");
    assert!(listed.contains(&recipient_id), "{listed}");
    assert!(listed.contains("/peer/cwd"), "{listed}");
    assert!(
        !listed.contains(&format!("\"{id}\"")),
        "a session does not list itself: {listed}"
    );
    assert!(
        !listed.contains("endpoint"),
        "no endpoint reaches the model"
    );

    let sent = call(
        &send,
        serde_json::json!({"session_id": recipient_id, "cwd": "/peer/cwd", "message": "rebase done"}),
    )
    .await
    .unwrap();
    assert!(sent.contains("\"accepted\""), "{sent}");
    let delivered = recipient.dequeue().expect("the message was queued");
    assert_eq!(delivered.sent, "rebase done");
    let sender = delivered.peer.expect("peer provenance");
    assert_eq!(sender.session_id, id, "the runtime names the sender");
    assert_eq!(sender.cwd, h.app.dir_cwd.display().to_string());

    let unknown = call(
        &send,
        serde_json::json!({"session_id": recipient_id, "cwd": "/elsewhere", "message": "hi"}),
    )
    .await
    .unwrap_err();
    assert!(
        unknown.to_string().contains("no running session"),
        "{unknown}"
    );
    recipient.close().await;
}

/// A send names a session that discovery found registered but not answering
/// by what it answered, rather than calling it not running — the two read
/// alike to the model, and only one of them is worth asking `sessions` about.
#[tokio::test]
async fn a_send_to_a_session_that_did_not_answer_says_why() {
    let _data_home = isolated_data_home();
    let own = peer("resolve-own").await;
    let other = peer("resolve-other").await;
    let target = other.descriptor();
    let (id, cwd) = (target.session_id.clone(), target.working_directory.clone());
    let resolve = |found| {
        super::super::messaging_tools::resolve_target(found, &own.descriptor().endpoint, &id, &cwd)
    };

    let live = hrdr_agent::messaging::Discovery {
        live: vec![own.descriptor(), target.clone()],
        unreachable: Vec::new(),
    };
    assert_eq!(resolve(live).unwrap(), target);

    let silent = hrdr_agent::messaging::Discovery {
        live: vec![own.descriptor()],
        unreachable: vec![(target.clone(), "answered Unavailable".into())],
    };
    let error = resolve(silent).unwrap_err().to_string();
    assert!(
        error.contains("registered but did not answer (answered Unavailable)"),
        "{error}"
    );

    let gone = hrdr_agent::messaging::Discovery {
        live: vec![own.descriptor()],
        unreachable: Vec::new(),
    };
    let error = resolve(gone).unwrap_err().to_string();
    assert!(error.contains("no running session"), "{error}");

    // This session is never its own target, even under the name it was asked for.
    let mut mirror = own.descriptor();
    mirror.session_id = id.clone();
    mirror.working_directory = cwd.clone();
    let error = resolve(hrdr_agent::messaging::Discovery {
        live: vec![mirror],
        unreachable: Vec::new(),
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("no running session"), "{error}");
    own.close().await;
    other.close().await;
}

#[tokio::test]
async fn two_sessions_with_one_id_and_cwd_are_not_guessed_between() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    started(&mut h, "ambiguous sender").await;
    let id = format!("twin-{}", std::process::id());
    let mut twins = Vec::new();
    for _ in 0..2 {
        twins.push(
            Messaging::start(
                UserDirectory::open().unwrap(),
                id.clone(),
                "twin".into(),
                "/twin".into(),
            )
            .await
            .unwrap(),
        );
    }
    let send = MessageSessionTool {
        view: h.app.messaging.view(),
    };
    let error = call(
        &send,
        serde_json::json!({"session_id": id, "cwd": "/twin", "message": "which?"}),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("more than one"), "{error}");
    for twin in &twins {
        assert!(twin.dequeue().is_none());
        twin.close().await;
    }
}

#[tokio::test]
async fn messaging_tools_follow_the_flag_and_need_a_running_session() {
    let _data_home = isolated_data_home();
    let names = |enabled: bool| {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = hrdr_agent::Agent::new(AgentConfig {
            cwd: dir.path().to_path_buf(),
            sandbox: hrdr_tools::SandboxMode::None,
            ..Default::default()
        })
        .unwrap();
        super::super::messaging::MessagingState::new(enabled).register_tools(&mut agent);
        agent
            .tools()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>()
    };
    let on = names(true);
    assert!(on.iter().any(|name| name == "sessions"));
    assert!(on.iter().any(|name| name == "message_session"));
    let off = names(false);
    assert!(
        !off.iter()
            .any(|name| name == "sessions" || name == "message_session")
    );

    let idle = super::super::messaging::MessagingState::new(true);
    let error = call(&SessionsTool { view: idle.view() }, serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not running"), "{error}");
}

/// A brand-new session gets its id as its first turn starts, so the model's
/// first reply can call a messaging tool before the start has landed.
#[tokio::test]
async fn a_tool_called_while_messaging_starts_waits_for_it() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![]).await;
    h.app.reserve_session_id("racing session");
    h.app.await_saves().await;
    h.app.sync_messaging();
    let sessions = SessionsTool {
        view: h.app.messaging.view(),
    };
    let listing = tokio::spawn(async move { call(&sessions, serde_json::json!({})).await });
    h.wait_for("messaging start", |msg| {
        matches!(msg, TurnMsg::MessagingStarted(..))
    })
    .await;
    let listed = listing
        .await
        .unwrap()
        .expect("the tool waited for the start");
    assert!(listed.contains("\"sessions\""), "{listed}");
}

/// Between a turn's end and its `Done` the agent is already idle, but the turn
/// still owns its handle. A peer message arriving then must wait for that
/// `Done` — which relaunches for anything queued — rather than start a turn the
/// late `Done` would then mistake for its own.
#[tokio::test]
async fn a_peer_message_arriving_before_done_is_run_after_it() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![MockReply::Text("handled".into())]).await;
    let id = started(&mut h, "done race").await;
    let sender = peer("done-race-sender").await;
    let target = find(&sender, &id).await.unwrap();
    assert_eq!(
        sender.send(&target, "check the logs".into()).await.unwrap(),
        SendStatus::Accepted
    );

    // The finished turn's `Done` has not been handled yet.
    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    h.app.turn_handle = Some(tokio::spawn(async {}));
    h.app.registry.end_turn(hrdr_agent::MAIN_KEY);
    assert!(!h.app.running());
    h.app.maybe_deliver_peer();
    assert_eq!(
        h.app.registry.pending(hrdr_agent::MAIN_KEY).len(),
        1,
        "queued behind the finishing turn"
    );
    assert!(!h.app.running(), "no turn is started under the old handle");

    h.app.on_turn_msg(TurnMsg::Done(None));
    assert!(
        h.app.running(),
        "Done relaunches for the queued peer message"
    );
    h.pump().await;
    let texts = peer_texts(&h);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains("check the logs"));
    sender.close().await;
}

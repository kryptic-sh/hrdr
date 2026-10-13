//! Session messaging through a real TUI app: discovery, idle wake, busy-agent
//! queueing, the disable flag and session rotation. The peer is a second runtime
//! in the same process, registered in the suite's private discovery directory
//! (`HRDR_IPC_DIR`, set by `hrdr-test-support`).

use super::*;
use hrdr_agent::messaging::{Messaging, SendStatus};
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

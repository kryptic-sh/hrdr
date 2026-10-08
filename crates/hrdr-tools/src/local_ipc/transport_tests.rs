use super::*;
use std::process::Stdio;

const DEADLINE: Duration = Duration::from_secs(10);
const PAYLOAD: &[u8] = b"native IPC\0\xff\nexact bytes";
const CHILD: &str = "local_ipc::transport::tests::native_child";

pub(super) fn test_root() -> tempfile::TempDir {
    // macOS's default per-user temp path can exceed sockaddr_un's pathname limit.
    // A fresh private directory under /tmp keeps real socket tests isolated and short.
    #[cfg(unix)]
    let root = tempfile::tempdir_in("/tmp");
    #[cfg(windows)]
    let root = tempfile::tempdir();
    root.unwrap()
}

#[test]
fn endpoint_validation_and_roundtrip() {
    let endpoint = EndpointId::fresh();
    let encoded = serde_json::to_value(&endpoint).unwrap();
    assert_eq!(
        serde_json::from_value::<EndpointId>(encoded.clone()).unwrap(),
        endpoint
    );
    assert_eq!(endpoint.pid(), std::process::id());
    let mut invalid = encoded.clone();
    invalid["pid"] = 0.into();
    assert!(serde_json::from_value::<EndpointId>(invalid).is_err());
    let mut invalid = encoded.clone();
    invalid["generation"] = serde_json::json!([0u8; 16].as_slice());
    assert!(serde_json::from_value::<EndpointId>(invalid).is_err());
    let mut invalid = encoded;
    invalid["path"] = "../replacement".into();
    assert!(serde_json::from_value::<EndpointId>(invalid).is_err());
    assert_ne!(EndpointId::fresh(), endpoint);
}

fn spawn_child(root: &std::path::Path, endpoint: &EndpointId, mode: &str) -> tokio::process::Child {
    tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD, "--nocapture"])
        .env("HRDR_IPC_CHILD_ROOT", root)
        .env(
            "HRDR_IPC_CHILD_ENDPOINT",
            serde_json::to_string(endpoint).unwrap(),
        )
        .env("HRDR_IPC_CHILD_MODE", mode)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

#[tokio::test]
async fn native_child() {
    let Some(root) = std::env::var_os("HRDR_IPC_CHILD_ROOT") else {
        return;
    };
    let endpoint: EndpointId =
        serde_json::from_str(&std::env::var("HRDR_IPC_CHILD_ENDPOINT").unwrap()).unwrap();
    let directory = UserDirectory::open_in(std::path::Path::new(&root)).unwrap();
    let mut connection = Connection::connect(&directory, &endpoint, DEADLINE)
        .await
        .unwrap();
    assert_eq!(connection.peer_pid(), endpoint.pid());
    let mode = std::env::var("HRDR_IPC_CHILD_MODE").unwrap();
    if mode != "echo" {
        // Wait for the parent's authenticated accept before sending malformed bytes.
        assert_eq!(connection.read_frame(DEADLINE).await.unwrap(), b"ready");
        match mode.as_str() {
            "oversized" => {
                connection
                    .stream
                    .as_mut()
                    .unwrap()
                    .write_u32(MAX_FRAME_BYTES as u32 + 1)
                    .await
                    .unwrap();
                assert!(connection.read_frame(DEADLINE).await.is_err());
            }
            "truncated" => {
                connection
                    .stream
                    .as_mut()
                    .unwrap()
                    .write_all(&[0, 0, 0, 3, 1])
                    .await
                    .unwrap();
            }
            "stall" => assert!(connection.read_frame(DEADLINE).await.is_err()),
            _ => panic!("unknown IPC child mode: {mode}"),
        }
        return;
    }
    connection.write_frame(PAYLOAD, DEADLINE).await.unwrap();
    assert_eq!(connection.read_frame(DEADLINE).await.unwrap(), PAYLOAD);
    connection.write_frame(b"ack", DEADLINE).await.unwrap();
}

#[tokio::test]
async fn native_cross_process_sequential_clients() {
    let root = test_root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let mut listener = Listener::bind(&directory).unwrap();
    for _ in 0..3 {
        let mut child = spawn_child(root.path(), listener.endpoint(), "echo");
        let child_pid = child.id().unwrap();
        let mut connection = listener.accept(DEADLINE).await.unwrap();
        assert_eq!(connection.peer_pid(), child_pid);
        assert_ne!(connection.peer_pid(), std::process::id());
        assert_eq!(connection.read_frame(DEADLINE).await.unwrap(), PAYLOAD);
        connection.write_frame(PAYLOAD, DEADLINE).await.unwrap();
        assert_eq!(connection.read_frame(DEADLINE).await.unwrap(), b"ack");
        assert!(
            timeout(DEADLINE, child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

#[tokio::test]
async fn native_cross_process_rejects_malformed_truncated_and_stalled_frames() {
    let root = test_root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let mut listener = Listener::bind(&directory).unwrap();
    for (mode, expected) in [
        ("oversized", io::ErrorKind::InvalidData),
        ("truncated", io::ErrorKind::UnexpectedEof),
        ("stall", io::ErrorKind::TimedOut),
    ] {
        let mut child = spawn_child(root.path(), listener.endpoint(), mode);
        let mut connection = listener.accept(DEADLINE).await.unwrap();
        assert_eq!(connection.peer_pid(), child.id().unwrap());
        connection.write_frame(b"ready", DEADLINE).await.unwrap();
        let deadline = if mode == "stall" {
            Duration::from_millis(50)
        } else {
            DEADLINE
        };
        assert_eq!(
            connection.read_frame(deadline).await.unwrap_err().kind(),
            expected
        );
        assert_eq!(
            connection.read_frame(DEADLINE).await.unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        assert!(
            timeout(DEADLINE, child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

#[tokio::test]
async fn refuses_spoofed_endpoint_pid_before_frames() {
    let root = test_root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let mut endpoint = EndpointId::fresh();
    endpoint.pid = endpoint.pid.checked_add(1).unwrap();
    let _imposter = platform::Listener::bind(
        &directory,
        &endpoint,
        &crate::local_ipc::storage::MutationGuard::acquire(&directory).unwrap(),
    )
    .unwrap();
    let error = Connection::connect(&directory, &endpoint, DEADLINE)
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

#[tokio::test]
async fn native_accept_deadline() {
    let root = test_root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let mut listener = Listener::bind(&directory).unwrap();
    assert_eq!(
        listener
            .accept(Duration::from_millis(20))
            .await
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::TimedOut
    );
    let client = Connection::connect(&directory, listener.endpoint(), DEADLINE)
        .await
        .unwrap();
    let accepted = listener.accept(DEADLINE).await.unwrap();
    assert_eq!(client.peer_pid(), std::process::id());
    assert_eq!(accepted.peer_pid(), std::process::id());
}

#[tokio::test]
async fn rejects_malformed_length_and_truncated_frames() {
    for (bytes, kind) in [
        (
            (MAX_FRAME_BYTES as u32 + 1).to_be_bytes().to_vec(),
            io::ErrorKind::InvalidData,
        ),
        (u32::MAX.to_be_bytes().to_vec(), io::ErrorKind::InvalidData),
        (vec![0, 0], io::ErrorKind::UnexpectedEof),
        (vec![0, 0, 0, 3, 1], io::ErrorKind::UnexpectedEof),
    ] {
        let (mut writer, stream) = tokio::io::duplex(32);
        writer.write_all(&bytes).await.unwrap();
        drop(writer);
        let mut connection = Connection::new(stream, 1);
        assert_eq!(
            connection.read_frame(DEADLINE).await.unwrap_err().kind(),
            kind
        );
        assert_eq!(
            connection.read_frame(DEADLINE).await.unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
    }
}

#[tokio::test]
async fn frame_boundaries_and_write_limit() {
    let (left, right) = tokio::io::duplex(4096);
    let mut writer = Connection::new(left, 1);
    let mut reader = Connection::new(right, 2);
    let payload = vec![0xa5; MAX_FRAME_BYTES];
    let (written, read) = tokio::join!(
        writer.write_frame(&payload, DEADLINE),
        reader.read_frame(DEADLINE)
    );
    written.unwrap();
    assert_eq!(read.unwrap(), payload);
    assert_eq!(
        writer
            .write_frame(&vec![0; MAX_FRAME_BYTES + 1], DEADLINE)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    writer.write_frame(&[], DEADLINE).await.unwrap();
    assert_eq!(reader.read_frame(DEADLINE).await.unwrap(), Vec::<u8>::new());
}

#[tokio::test]
async fn io_deadlines_close_partial_connections() {
    let (mut left, right) = tokio::io::duplex(8);
    let mut reader = Connection::new(right, 1);
    left.write_all(&[0, 0, 0, 3, 1]).await.unwrap();
    assert_eq!(
        reader
            .read_frame(Duration::from_millis(20))
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    assert_eq!(
        reader.read_frame(DEADLINE).await.unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
    let (left, _right) = tokio::io::duplex(8);
    let mut writer = Connection::new(left, 1);
    assert_eq!(
        writer
            .write_frame(&[1; 64], Duration::from_millis(20))
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    assert_eq!(
        writer.write_frame(&[], DEADLINE).await.unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
}

#[tokio::test]
async fn cancelled_frame_closes_connection() {
    let (mut left, right) = tokio::io::duplex(8);
    let mut reader = Connection::new(right, 1);
    left.write_all(&[0, 0]).await.unwrap();
    assert!(
        timeout(Duration::from_millis(20), reader.read_frame(DEADLINE))
            .await
            .is_err()
    );
    assert_eq!(
        reader.read_frame(DEADLINE).await.unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
}

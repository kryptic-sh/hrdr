use std::{
    fs::File,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions},
};
use windows_sys::Win32::{
    Foundation::ERROR_PIPE_BUSY,
    System::{
        Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
        Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

use super::{Connection, EndpointId, UserDirectory, verify_pid};
use crate::local_ipc::{
    insecure,
    platform::{User, with_security_attributes},
};

struct Pipe<T: AsRawHandle>(T);

impl<T: AsRawHandle> Drop for Pipe<T> {
    fn drop(&mut self) {
        // SAFETY: the Tokio pipe still owns the handle. Null cancels all pending
        // operations, including Mio's buffered write. Mio retains its Arc, buffer
        // and OVERLAPPED until IOCP completion; cancellation does not free them.
        let result = unsafe {
            windows_sys::Win32::System::IO::CancelIoEx(self.0.as_raw_handle(), std::ptr::null())
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(windows_sys::Win32::Foundation::ERROR_NOT_FOUND as i32)
            {
                eprintln!("failed to cancel IPC pipe IO: {error}");
            }
        }
    }
}

impl<T: AsRawHandle + AsyncRead + Unpin> AsyncRead for Pipe<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<T: AsRawHandle + AsyncWrite + Unpin> AsyncWrite for Pipe<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Tokio's flush is a no-op. Mio rejects even an empty write while a
        // previous write is outstanding, and delivers its completion error here.
        // Unlike readiness alone, this clears stale readiness and waits for IOCP.
        // The new empty write carries no frame bytes; drop cancels it if pending.
        Pin::new(&mut self.0).poll_write(cx, &[]).map_ok(|_| ())
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

pub(super) struct Listener {
    pending: NamedPipeServer,
    name: String,
    sddl: String,
    user: User,
    _directory: File,
}

fn name(endpoint: &EndpointId) -> String {
    format!(r"\\.\pipe\hrdr-ipc-v1-{}", endpoint.name())
}

fn instance(name: &str, sddl: &str, first: bool) -> io::Result<NamedPipeServer> {
    with_security_attributes(sddl, |attributes| {
        // SAFETY: helper retains the descriptor and attributes through this
        // synchronous CreateNamedPipe call; nothing stores their pointers.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(name, std::ptr::from_mut(attributes).cast())
        }
    })
}

impl Listener {
    pub(super) fn bind(
        directory: &UserDirectory,
        endpoint: &EndpointId,
        _mutation: &crate::local_ipc::storage::MutationGuard,
    ) -> io::Result<Self> {
        let user = User::current()?;
        let sddl = user.sddl()?;
        let name = name(endpoint);
        Ok(Self {
            pending: instance(&name, &sddl, true)?,
            name,
            sddl,
            user,
            _directory: directory._directory.try_clone()?,
        })
    }

    pub(super) async fn accept(&mut self) -> io::Result<Connection> {
        self.accept_authenticated(|stream, user| authenticate(stream, true, None, user))
            .await
    }

    async fn accept_authenticated(
        &mut self,
        authenticate: impl FnOnce(&NamedPipeServer, &User) -> io::Result<u32>,
    ) -> io::Result<Connection> {
        self.pending.connect().await?;
        // Keep the name occupied continuously, including on authentication failure.
        // Every replacement instance receives the same explicit private DACL.
        let next = instance(&self.name, &self.sddl, false)?;
        let stream = std::mem::replace(&mut self.pending, next);
        let pid = authenticate(&stream, &self.user).map_err(super::rejected_peer)?;
        Ok(Connection::new(Pipe(stream), pid))
    }
}

pub(super) async fn connect(
    _directory: &UserDirectory,
    endpoint: &EndpointId,
) -> io::Result<Connection> {
    let user = User::current()?;
    let name = name(endpoint);
    let stream = loop {
        // Tokio defaults to SECURITY_IDENTIFICATION, preventing a malicious server
        // from using the client's token for impersonation.
        match ClientOptions::new().open(&name) {
            Ok(stream) => break stream,
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error),
        }
    };
    let pid = authenticate(&stream, false, Some(endpoint.pid()), &user)?;
    Ok(Connection::new(Pipe(stream), pid))
}

fn process_open_error(error: io::Error) -> io::Error {
    // The kernel-reported peer PID can disappear before OpenProcess runs.
    if error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER as i32)
    {
        io::Error::new(io::ErrorKind::ConnectionAborted, error)
    } else {
        error
    }
}

fn authenticate(
    pipe: &impl AsRawHandle,
    server: bool,
    expected: Option<u32>,
    user: &User,
) -> io::Result<u32> {
    let mut pid = 0;
    // SAFETY: the stream retains the pipe handle; pid is writable output storage.
    let success = unsafe {
        if server {
            GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid)
        } else {
            GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid)
        }
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    verify_pid(pid, expected)?;
    // SAFETY: requests a noninherited query-only handle for the kernel-reported PID.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return Err(process_open_error(io::Error::last_os_error()));
    }
    // SAFETY: successful OpenProcess transferred ownership; retain through SID check.
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    if !user.same_user(&User::of_process(&process)?) {
        return Err(insecure());
    }
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_ipc::platform::validate;

    #[tokio::test]
    async fn exited_peer_authentication_preserves_listener() {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};

        let root = tempfile::tempdir().unwrap();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let endpoint = EndpointId::fresh();
        let mut listener = Listener::bind(
            &directory,
            &endpoint,
            &crate::local_ipc::storage::MutationGuard::acquire(&directory).unwrap(),
        )
        .unwrap();
        let peer = connect(&directory, &endpoint).await.unwrap();
        let rejected = listener
            .accept_authenticated(|_, _| {
                let error = io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER as i32);
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
                Err(process_open_error(error))
            })
            .await
            .err()
            .unwrap();
        assert_eq!(rejected.kind(), io::ErrorKind::ConnectionAborted);
        drop(peer);
        let denied = process_open_error(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32));
        assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(denied.raw_os_error(), Some(ERROR_ACCESS_DENIED as i32));
        let mut peer = connect(&directory, &endpoint).await.unwrap();
        let mut accepted = listener.accept().await.unwrap();
        let (sent, received) = tokio::join!(
            peer.write_frame(b"after rejection", Duration::from_secs(2)),
            accepted.read_frame(Duration::from_secs(2)),
        );
        sent.unwrap();
        assert_eq!(received.unwrap(), b"after rejection");
    }

    #[tokio::test]
    async fn every_instance_is_private_and_name_remains_reserved() {
        let root = tempfile::tempdir().unwrap();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let endpoint = EndpointId::fresh();
        let mut listener = Listener::bind(
            &directory,
            &endpoint,
            &crate::local_ipc::storage::MutationGuard::acquire(&directory).unwrap(),
        )
        .unwrap();
        assert!(
            Listener::bind(
                &directory,
                &endpoint,
                &crate::local_ipc::storage::MutationGuard::acquire(&directory).unwrap()
            )
            .is_err()
        );
        for _ in 0..3 {
            validate(&listener.pending, &listener.user).unwrap();
            assert!(instance(&listener.name, &listener.sddl, true).is_err());
            let _client = connect(&directory, &endpoint).await.unwrap();
            let accepted = listener.accept().await.unwrap();
            drop(accepted);
            assert!(instance(&listener.name, &listener.sddl, true).is_err());
        }
        validate(&listener.pending, &listener.user).unwrap();
    }

    #[tokio::test]
    async fn authenticates_process_token_sid_in_both_directions() {
        let root = tempfile::tempdir().unwrap();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let endpoint = EndpointId::fresh();
        let listener = Listener::bind(
            &directory,
            &endpoint,
            &crate::local_ipc::storage::MutationGuard::acquire(&directory).unwrap(),
        )
        .unwrap();
        let client = ClientOptions::new().open(&listener.name).unwrap();
        listener.pending.connect().await.unwrap();
        let different = User::different_for_test();
        assert_eq!(
            authenticate(&client, false, Some(endpoint.pid()), &different)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            authenticate(&listener.pending, true, None, &different)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            authenticate(&client, false, Some(endpoint.pid()), &listener.user).unwrap(),
            std::process::id()
        );
        assert_eq!(
            authenticate(&listener.pending, true, None, &listener.user).unwrap(),
            std::process::id()
        );
    }

    #[tokio::test]
    async fn backpressure_write_timeout_releases_native_handle() {
        let root = tempfile::tempdir().unwrap();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let endpoint = EndpointId::fresh();
        let mut listener = Listener::bind(
            &directory,
            &endpoint,
            &crate::local_ipc::storage::MutationGuard::acquire(&directory).unwrap(),
        )
        .unwrap();
        let mut peer = ClientOptions::new().open(&listener.name).unwrap();
        let mut sender = listener.accept().await.unwrap();
        let payload = vec![42; super::super::MAX_FRAME_BYTES];
        let result = sender
            .write_frame(&payload, Duration::from_millis(50))
            .await;
        // Check the native resource before the result: the old implementation
        // reports success with an outstanding write, and drop leaves it alive.
        drop(sender);
        assert_server_end_closed(&mut peer).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn dropped_backpressured_write_releases_native_handle() {
        let root = tempfile::tempdir().unwrap();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let endpoint = EndpointId::fresh();
        let mut listener = Listener::bind(
            &directory,
            &endpoint,
            &crate::local_ipc::storage::MutationGuard::acquire(&directory).unwrap(),
        )
        .unwrap();
        let mut peer = ClientOptions::new().open(&listener.name).unwrap();
        let mut sender = listener.accept().await.unwrap();
        let payload = vec![42; super::super::MAX_FRAME_BYTES];
        let mut write = Box::pin(sender.write_frame(&payload, Duration::from_secs(30)));
        tokio::select! {
            biased;
            result = &mut write => panic!("backpressured write finished: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        drop(write);
        assert_server_end_closed(&mut peer).await;
    }

    /// The server's pipe handle is closed once the client reads past whatever
    /// was buffered to the end of the pipe. While a pending write still holds
    /// the handle, the read just waits. Polling the handle *value* instead
    /// cannot tell: Windows reissues a freed value at once, and parallel tests
    /// keep opening handles.
    async fn assert_server_end_closed(peer: &mut tokio::net::windows::named_pipe::NamedPipeClient) {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0; 64 * 1024];
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match peer.read(&mut buf).await {
                    Ok(0) => return,
                    Ok(_) => continue,
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return,
                    Err(error) => panic!("reading the peer end: {error}"),
                }
            }
        })
        .await
        .expect("pending native write retained its pipe handle");
    }

    #[tokio::test]
    async fn connect_deadline_when_all_instances_are_busy() {
        let root = tempfile::tempdir().unwrap();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let endpoint = EndpointId::fresh();
        let listener = Listener::bind(
            &directory,
            &endpoint,
            &crate::local_ipc::storage::MutationGuard::acquire(&directory).unwrap(),
        )
        .unwrap();
        let _client = ClientOptions::new().open(&listener.name).unwrap();
        listener.pending.connect().await.unwrap();
        let error = Connection::connect(&directory, &endpoint, Duration::from_millis(20))
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}

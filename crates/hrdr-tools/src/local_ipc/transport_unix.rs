use std::{
    fs::{File, Metadata, OpenOptions},
    io,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use tokio::net::{UnixListener, UnixStream};

use super::{Connection, EndpointId, UserDirectory, verify_pid};
use crate::local_ipc::{insecure, platform as directory_security};

pub(super) struct Listener {
    socket: UnixListener,
    directory: File,
    directory_path: PathBuf,
}

fn identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

// Unix socket APIs require pathnames (including on macOS). Check the retained
// directory before and after pathname operations; never accept a substituted
// directory. The trusted parent contract of UserDirectory::open_in also applies
// here: it must not be concurrently renamed by its owner during a pathname
// syscall. These checks cannot make pathname resolution atomic with fstat.
fn revalidate(directory: &File, path: &Path) -> io::Result<()> {
    directory_security::validate(directory)?;
    let current = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if identity(&directory.metadata()?) != identity(&current.metadata()?) {
        return Err(insecure());
    }
    directory_security::validate(&current)
}

impl Listener {
    pub(super) fn bind(
        directory: &UserDirectory,
        endpoint: &EndpointId,
        _mutation: &crate::local_ipc::storage::MutationGuard,
    ) -> io::Result<Self> {
        let retained = directory._directory.try_clone()?;
        revalidate(&retained, &directory.path)?;
        let path = directory.path.join(endpoint.name());
        let socket = UnixListener::bind(&path)?;
        revalidate(&retained, &directory.path)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_socket() {
            return Err(insecure());
        }
        Ok(Self {
            socket,
            directory: retained,
            directory_path: directory.path.clone(),
        })
    }

    pub(super) async fn accept(&mut self) -> io::Result<Connection> {
        revalidate(&self.directory, &self.directory_path)?;
        let (stream, _) = self.socket.accept().await?;
        revalidate(&self.directory, &self.directory_path)?;
        let pid = authenticate(&stream, None)?;
        Ok(Connection::new(stream, pid))
    }
}

// No pathname cleanup on drop: the next guarded scan reclaims the generation
// only after its independent kernel lease can be locked.

pub(super) async fn connect(
    directory: &UserDirectory,
    endpoint: &EndpointId,
) -> io::Result<Connection> {
    revalidate(&directory._directory, &directory.path)?;
    let path = directory.path.join(endpoint.name());
    let before = std::fs::symlink_metadata(&path)?;
    if !before.file_type().is_socket() {
        return Err(insecure());
    }
    let stream = UnixStream::connect(&path).await?;
    revalidate(&directory._directory, &directory.path)?;
    if identity(&before) != identity(&std::fs::symlink_metadata(&path)?) {
        return Err(insecure());
    }
    let pid = authenticate(&stream, Some(endpoint.pid()))?;
    Ok(Connection::new(stream, pid))
}

fn authenticate(stream: &UnixStream, expected: Option<u32>) -> io::Result<u32> {
    let credentials = stream.peer_cred()?;
    // SAFETY: geteuid has no preconditions.
    if credentials.uid() != unsafe { libc::geteuid() } {
        return Err(insecure());
    }
    let pid = credentials
        .pid()
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(insecure)?;
    verify_pid(pid, expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_ipc::storage::MutationGuard;
    use std::os::unix::fs::{DirBuilderExt, symlink};

    #[tokio::test]
    async fn refuses_existing_endpoint_and_preserves_replacement_socket() {
        let root = super::super::tests::test_root();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let endpoint = EndpointId::fresh();
        let listener = Listener::bind(
            &directory,
            &endpoint,
            &MutationGuard::acquire(&directory).unwrap(),
        )
        .unwrap();
        let path = directory.path.join(endpoint.name());
        assert!(
            Listener::bind(
                &directory,
                &endpoint,
                &MutationGuard::acquire(&directory).unwrap()
            )
            .is_err()
        );
        let moved = directory.path.join("old-socket");
        std::fs::rename(&path, &moved).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        let original = identity(&std::fs::symlink_metadata(&path).unwrap());
        drop(listener);
        assert_eq!(
            identity(&std::fs::symlink_metadata(&path).unwrap()),
            original
        );
        drop(replacement);
    }

    #[tokio::test]
    async fn refuses_endpoint_symlink_even_to_same_process() {
        let root = super::super::tests::test_root();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let endpoint = EndpointId::fresh();
        let _listener = Listener::bind(
            &directory,
            &endpoint,
            &MutationGuard::acquire(&directory).unwrap(),
        )
        .unwrap();
        let alias = EndpointId::fresh();
        symlink(
            directory.path.join(endpoint.name()),
            directory.path.join(alias.name()),
        )
        .unwrap();
        assert_eq!(
            connect(&directory, &alias).await.err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn drop_leaves_dead_endpoint_without_unlinking() {
        let root = super::super::tests::test_root();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let endpoint = EndpointId::fresh();
        let listener = Listener::bind(
            &directory,
            &endpoint,
            &MutationGuard::acquire(&directory).unwrap(),
        )
        .unwrap();
        let path = directory.path.join(endpoint.name());
        let original = identity(&std::fs::symlink_metadata(&path).unwrap());
        drop(listener);
        assert_eq!(
            identity(&std::fs::symlink_metadata(&path).unwrap()),
            original
        );
        assert!(connect(&directory, &endpoint).await.is_err());
        assert!(
            Listener::bind(
                &directory,
                &endpoint,
                &MutationGuard::acquire(&directory).unwrap()
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn refuses_replaced_directory_for_bind_connect_accept_and_cleanup() {
        let root = super::super::tests::test_root();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let endpoint = EndpointId::fresh();
        let mut listener = Listener::bind(
            &directory,
            &endpoint,
            &MutationGuard::acquire(&directory).unwrap(),
        )
        .unwrap();
        std::fs::rename(&directory.path, root.path().join("moved")).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory.path)
            .unwrap();
        let replacement = directory.path.join(endpoint.name());
        std::fs::write(&replacement, b"replacement").unwrap();
        assert_eq!(
            Listener::bind(
                &directory,
                &EndpointId::fresh(),
                &MutationGuard::acquire(&directory).unwrap()
            )
            .err()
            .unwrap()
            .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            connect(&directory, &endpoint).await.err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            listener.accept().await.err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        drop(listener);
        assert_eq!(std::fs::read(replacement).unwrap(), b"replacement");
    }

    #[tokio::test]
    async fn refuses_directory_symlink_replacement() {
        let root = super::super::tests::test_root();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let moved = root.path().join("moved");
        std::fs::rename(&directory.path, &moved).unwrap();
        symlink(&moved, &directory.path).unwrap();
        assert!(
            Listener::bind(
                &directory,
                &EndpointId::fresh(),
                &MutationGuard::acquire(&directory).unwrap()
            )
            .is_err()
        );
    }
}

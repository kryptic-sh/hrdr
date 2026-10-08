//! Private discovery-directory foundation for local IPC.
//!
//! Opening validates an existing directory rather than repairing its permissions.
//! No registration-file or transport operations are implemented here.

use std::{fs::File, io, path::Path};

#[cfg(unix)]
#[path = "unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

/// An open, validated per-user discovery directory.
///
/// The retained handle identifies the validated object even if its pathname changes.
/// Future registration reads and atomic writes must be anchored to this handle, not
/// reopen an unchecked pathname. This type deliberately exposes no file operations yet.
#[derive(Debug)]
pub struct UserDirectory {
    _directory: File,
}

impl UserDirectory {
    /// Open the platform's private discovery directory, creating it if absent.
    /// Existing insecure directories are rejected without changing their permissions.
    pub fn open() -> io::Result<Self> {
        Self::open_in(&platform::system_parent()?)
    }

    /// Open under an existing trusted parent instead of the system location.
    ///
    /// Intended for isolated test roots. The caller must control the parent or trust
    /// its protection against replacement; the final child is independently validated.
    /// Canonicalizing the parent permits the system `/tmp` symlink on macOS.
    pub fn open_in(parent: &Path) -> io::Result<Self> {
        Ok(Self {
            _directory: platform::open(&parent.canonicalize()?)?,
        })
    }
}

fn insecure() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "insecure IPC directory")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_reopens_private_directory() {
        let root = tempfile::tempdir().unwrap();
        let first = UserDirectory::open_in(root.path()).unwrap();
        let second = UserDirectory::open_in(root.path()).unwrap();
        assert!(first._directory.metadata().unwrap().is_dir());
        assert!(second._directory.metadata().unwrap().is_dir());
    }

    #[test]
    fn rejects_regular_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(platform::name()), b"not a directory").unwrap();
        assert!(UserDirectory::open_in(root.path()).is_err());
    }
}

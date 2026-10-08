use std::{
    fs::{DirBuilder, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub(super) fn system_parent() -> io::Result<PathBuf> {
    Ok(PathBuf::from("/tmp"))
}

pub(super) fn name() -> String {
    // SAFETY: geteuid has no pointer arguments or preconditions.
    format!("hrdr-ipc-{}", unsafe { libc::geteuid() })
}

pub(super) fn open(parent: &Path) -> io::Result<File> {
    let path = parent.join(name());
    match DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    // File::metadata uses fstat on the retained descriptor, not the pathname.
    let metadata = directory.metadata()?;
    // SAFETY: geteuid has no pointer arguments or preconditions.
    let uid = unsafe { libc::geteuid() };
    validate_metadata(&metadata, uid)?;
    #[cfg(target_os = "macos")]
    macos_acl::reject_extended_acl(&directory)?;
    Ok(directory)
}

fn validate_metadata(metadata: &std::fs::Metadata, uid: libc::uid_t) -> io::Result<()> {
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o7777 != 0o700 {
        return Err(super::insecure());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod macos_acl {
    use super::*;
    use std::{ffi::c_void, os::fd::AsRawFd, ptr::NonNull};

    // libc lacks these bindings. ABI: Apple's Libc include/sys/acl.h.
    const ACL_TYPE_EXTENDED: libc::c_int = 0x100;
    const ACL_FIRST_ENTRY: libc::c_int = 0;

    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> *mut c_void;
        fn acl_valid(acl: *mut c_void) -> libc::c_int;
        fn acl_get_entry(
            acl: *mut c_void,
            entry_id: libc::c_int,
            entry: *mut *mut c_void,
        ) -> libc::c_int;
        fn acl_free(object: *mut c_void) -> libc::c_int;
    }

    struct Acl(NonNull<c_void>);

    impl Drop for Acl {
        fn drop(&mut self) {
            // SAFETY: this allocation came from acl_get_fd_np and is freed once.
            // Apple's acl_free always returns zero for an owned allocation.
            unsafe { acl_free(self.0.as_ptr()) };
        }
    }

    pub(super) fn reject_extended_acl(directory: &File) -> io::Result<()> {
        // SAFETY: the borrowed File keeps its descriptor live throughout this call.
        let acl =
            Acl(
                NonNull::new(unsafe { acl_get_fd_np(directory.as_raw_fd(), ACL_TYPE_EXTENDED) })
                    .ok_or_else(io::Error::last_os_error)?,
            );
        // SAFETY: acl owns a live ACL allocation.
        if unsafe { acl_valid(acl.0.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut entry = std::ptr::null_mut();
        // SAFETY: acl is valid and entry points to writable pointer storage.
        let result = unsafe { acl_get_entry(acl.0.as_ptr(), ACL_FIRST_ENTRY, &mut entry) };
        if result == 0 {
            return Err(super::super::insecure());
        }
        let error = io::Error::last_os_error();
        // Darwin reports end-of-ACL as EINVAL, unlike Linux. With a validated
        // ACL and ACL_FIRST_ENTRY, this means the ACL has no entries.
        if result == -1 && error.raw_os_error() == Some(libc::EINVAL) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_ipc::UserDirectory;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn creates_with_current_owner_and_rejects_wrong_mode() {
        let root = tempfile::tempdir().unwrap();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let metadata = directory._directory.metadata().unwrap();
        // SAFETY: geteuid has no preconditions.
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o7777, 0o700);
        std::fs::set_permissions(
            root.path().join(name()),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(UserDirectory::open_in(root.path()).is_err());
    }

    #[test]
    fn metadata_validator_rejects_different_expected_uid() {
        let root = tempfile::tempdir().unwrap();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let metadata = directory._directory.metadata().unwrap();
        validate_metadata(&metadata, metadata.uid()).unwrap();
        // Exercises the validator, not opening a genuinely foreign-owned directory.
        let different_uid = metadata.uid() ^ 1;
        assert_eq!(
            validate_metadata(&metadata, different_uid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn retained_handle_keeps_identity_after_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let original = directory._directory.metadata().unwrap();
        let path = root.path().join(name());
        let moved = root.path().join("moved");
        std::fs::rename(&path, &moved).unwrap();
        DirBuilder::new().mode(0o700).create(&path).unwrap();
        let retained = directory._directory.metadata().unwrap();
        let renamed = std::fs::metadata(moved).unwrap();
        let replacement = std::fs::metadata(path).unwrap();
        let identity = |metadata: &std::fs::Metadata| (metadata.dev(), metadata.ino());
        assert_eq!(identity(&retained), identity(&original));
        assert_eq!(identity(&retained), identity(&renamed));
        assert_ne!(identity(&retained), identity(&replacement));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rejects_everyone_allow_acl_even_with_private_mode() {
        let root = tempfile::tempdir().unwrap();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let path = root.path().join(name());
        let output = std::process::Command::new("/bin/chmod")
            .arg("+a")
            .arg("everyone allow list,search")
            .arg(&path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let metadata = directory._directory.metadata().unwrap();
        // SAFETY: geteuid has no preconditions.
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o7777, 0o700);
        assert_eq!(
            UserDirectory::open_in(root.path()).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn rejects_symlink_but_accepts_trusted_parent_symlink() {
        let root = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        symlink(target.path(), root.path().join(name())).unwrap();
        assert!(UserDirectory::open_in(root.path()).is_err());
        let links = tempfile::tempdir().unwrap();
        symlink(target.path(), links.path().join("parent")).unwrap();
        UserDirectory::open_in(&links.path().join("parent")).unwrap();
    }
}

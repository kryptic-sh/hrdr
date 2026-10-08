use super::{Snapshot, UserDirectory, push_name};
use crate::local_ipc::platform;
use std::{
    ffi::{CStr, CString, OsString},
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::{ffi::OsStringExt, fs::MetadataExt},
    },
    ptr::NonNull,
};

fn open(directory: &UserDirectory, name: &str, flags: i32) -> io::Result<File> {
    platform::validate(&directory._directory)?;
    let name = CString::new(name).map_err(io::Error::other)?;
    // SAFETY: retained directory and terminated name remain live during openat.
    let fd = unsafe {
        libc::openat(
            directory._directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat transfers a fresh descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn validate(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(crate::local_ipc::insecure());
    }
    #[cfg(target_os = "macos")]
    platform::macos_acl::reject_extended_acl(file)?;
    Ok(())
}

pub(in crate::local_ipc) fn create(directory: &UserDirectory, name: &str) -> io::Result<File> {
    let file = open(directory, name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL)?;
    validate(&file)?;
    Ok(file)
}

pub(in crate::local_ipc) fn read(directory: &UserDirectory, name: &str) -> io::Result<File> {
    let file = open(directory, name, libc::O_RDONLY)?;
    validate(&file)?;
    Ok(file)
}

pub(super) fn read_write(directory: &UserDirectory, name: &str) -> io::Result<File> {
    let file = open(directory, name, libc::O_RDWR)?;
    validate(&file)?;
    Ok(file)
}

pub(super) fn publish(
    _mutation: &super::MutationGuard,
    directory: &UserDirectory,
    endpoint: &super::EndpointId,
) -> io::Result<()> {
    platform::validate(&directory._directory)?;
    let temporary = CString::new(format!("{}.tmp", endpoint.name())).map_err(io::Error::other)?;
    let destination =
        CString::new(format!("{}.json", endpoint.name())).map_err(io::Error::other)?;
    let fd = directory._directory.as_raw_fd();
    // SAFETY: both names are terminated and the directory descriptor is retained.
    if unsafe { libc::renameat(fd, temporary.as_ptr(), fd, destination.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn validate_entry(
    directory: &UserDirectory,
    name: &str,
    socket: bool,
) -> io::Result<bool> {
    if !socket {
        return match read(directory, name) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        };
    }
    platform::validate(&directory._directory)?;
    let name = CString::new(name).map_err(io::Error::other)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: retained fd, terminated name, and writable stat output are live.
    if unsafe {
        libc::fstatat(
            directory._directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(error)
        };
    }
    // SAFETY: successful fstatat initialized stat; geteuid has no preconditions.
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFSOCK || stat.st_uid != unsafe { libc::geteuid() } {
        return Err(crate::local_ipc::insecure());
    }
    Ok(true)
}

pub(super) fn remove(directory: &UserDirectory, name: &str) -> io::Result<()> {
    platform::validate(&directory._directory)?;
    let name = CString::new(name).map_err(io::Error::other)?;
    // SAFETY: retained fd and terminated name stay live. The caller holds the
    // cooperative mutation lock and has validated this exact generation entry.
    if unsafe { libc::unlinkat(directory._directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct Directory(NonNull<libc::DIR>);
impl Drop for Directory {
    fn drop(&mut self) {
        // SAFETY: fdopendir transferred this uniquely owned stream.
        unsafe {
            libc::closedir(self.0.as_ptr());
        }
    }
}

pub(in crate::local_ipc) fn names(directory: &UserDirectory) -> io::Result<Snapshot> {
    // A new open file description avoids sharing/reusing directory iteration offsets.
    let file = open(directory, ".", libc::O_RDONLY | libc::O_DIRECTORY)?;
    let fd = file.into_raw_fd();
    // SAFETY: fd is owned and fdopendir consumes it only on success.
    let Some(stream) = NonNull::new(unsafe { libc::fdopendir(fd) }) else {
        let error = io::Error::last_os_error();
        // SAFETY: failed fdopendir did not consume fd.
        drop(unsafe { File::from_raw_fd(fd) });
        return Err(error);
    };
    let stream = Directory(stream);
    let mut names = Snapshot::new();
    loop {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: returns this thread's errno storage.
        let errno = unsafe { libc::__errno_location() };
        #[cfg(target_os = "macos")]
        // SAFETY: returns this thread's errno storage.
        let errno = unsafe { libc::__error() };
        #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
        compile_error!("IPC directory iteration requires a platform errno accessor");
        // SAFETY: errno is thread-local; stream is uniquely owned and live. readdir's
        // borrowed entry is copied before the next call, and never shared with a thread.
        let entry = unsafe {
            *errno = 0;
            libc::readdir(stream.0.as_ptr())
        };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                return Err(error);
            }
            break;
        }
        // SAFETY: successful readdir returns a terminated d_name in its live entry.
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes != b"."
            && bytes != b".."
            && !push_name(&mut names, OsString::from_vec(bytes.to_vec()))
        {
            break;
        }
    }
    Ok(names)
}

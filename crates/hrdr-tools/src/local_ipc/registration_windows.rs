use super::{Snapshot, UserDirectory, push_name};
use crate::local_ipc::platform;
use std::{
    fs::{File, OpenOptions},
    io,
    mem::zeroed,
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE,
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, READ_CONTROL,
    },
};

fn info(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    // SAFETY: this Win32 output struct permits zero initialization.
    let mut info = unsafe { zeroed() };
    // SAFETY: retained handle and writable output remain valid throughout the call.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

fn revalidate(directory: &UserDirectory) -> io::Result<()> {
    let current = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&directory.path)?;
    let retained = info(&directory._directory)?;
    let observed = info(&current)?;
    let identity = |info: &BY_HANDLE_FILE_INFORMATION| {
        (
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        )
    };
    if identity(&retained) != identity(&observed)
        || observed.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
            != FILE_ATTRIBUTE_DIRECTORY
    {
        return Err(crate::local_ipc::insecure());
    }
    let user = platform::User::current()?;
    platform::validate(&directory._directory, &user)?;
    platform::validate(&current, &user)
}

fn validate(file: &File) -> io::Result<()> {
    if info(file)?.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || !file.metadata()?.is_file()
    {
        return Err(crate::local_ipc::insecure());
    }
    platform::validate(file, &platform::User::current()?)
}

// The retained directory denies delete sharing. Path operations additionally check
// identity on both sides, under UserDirectory::open_in's trusted-parent contract.
pub(in crate::local_ipc) fn create(directory: &UserDirectory, name: &str) -> io::Result<File> {
    revalidate(directory)?;
    let path = platform::wide(&directory.path.join(name))?;
    let file =
        platform::with_security_attributes(&platform::User::current()?.sddl()?, |attributes| {
            // SAFETY: terminated path and owned security descriptor outlive CreateFileW.
            let raw = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    FILE_GENERIC_READ | FILE_GENERIC_WRITE | READ_CONTROL,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    attributes,
                    CREATE_NEW,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    null_mut(),
                )
            };
            if raw == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful CreateFileW transfers a fresh handle.
            Ok(unsafe { File::from_raw_handle(raw) })
        })?;
    validate(&file)?;
    revalidate(directory)?;
    Ok(file)
}

pub(in crate::local_ipc) fn read(directory: &UserDirectory, name: &str) -> io::Result<File> {
    open_existing(directory, name, FILE_GENERIC_READ)
}

pub(super) fn read_write(directory: &UserDirectory, name: &str) -> io::Result<File> {
    open_existing(directory, name, FILE_GENERIC_READ | FILE_GENERIC_WRITE)
}

fn open_existing(directory: &UserDirectory, name: &str, access: u32) -> io::Result<File> {
    revalidate(directory)?;
    let path = platform::wide(&directory.path.join(name))?;
    // SAFETY: terminated path; OPEN_REPARSE_POINT opens links themselves, not targets.
    let raw = unsafe {
        CreateFileW(
            path.as_ptr(),
            access | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreateFileW transfers a fresh handle.
    let file = unsafe { File::from_raw_handle(raw) };
    validate(&file)?;
    revalidate(directory)?;
    Ok(file)
}

pub(super) fn publish(
    _mutation: &super::MutationGuard,
    directory: &UserDirectory,
    endpoint: &super::EndpointId,
) -> io::Result<()> {
    revalidate(directory)?;
    std::fs::rename(
        directory.path.join(format!("{}.tmp", endpoint.name())),
        directory.path.join(format!("{}.json", endpoint.name())),
    )?;
    revalidate(directory)
}

pub(super) fn validate_entry(
    directory: &UserDirectory,
    name: &str,
    socket: bool,
) -> io::Result<bool> {
    if socket {
        revalidate(directory)?;
        return match std::fs::symlink_metadata(directory.path.join(name)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
            Ok(_) => Err(crate::local_ipc::insecure()),
        };
    }
    match read(directory, name) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn remove(directory: &UserDirectory, name: &str) -> io::Result<()> {
    revalidate(directory)?;
    std::fs::remove_file(directory.path.join(name))?;
    revalidate(directory)
}

pub(in crate::local_ipc) fn names(directory: &UserDirectory) -> io::Result<Snapshot> {
    revalidate(directory)?;
    let mut names = Snapshot::new();
    for entry in std::fs::read_dir(&directory.path)? {
        if !push_name(&mut names, entry?.file_name()) {
            break;
        }
    }
    revalidate(directory)?;
    Ok(names)
}

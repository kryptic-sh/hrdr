use std::{
    ffi::{OsString, c_void},
    fs::File,
    io,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_TOKEN, INVALID_HANDLE_VALUE,
        LocalFree,
    },
    Security::{
        ACCESS_ALLOWED_ACE, ACL,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SE_FILE_OBJECT,
        },
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl,
        GetTokenInformation, IsValidAcl, OWNER_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED,
        SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ALL_ACCESS,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileInformationByHandle, OPEN_EXISTING, READ_CONTROL,
    },
    System::{
        Com::CoTaskMemFree,
        Threading::{GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken},
    },
    UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath},
};

struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns only allocations returned by LocalAlloc-family APIs.
        unsafe {
            LocalFree(self.0);
        }
    }
}

struct KnownFolder(*mut u16);
impl Drop for KnownFolder {
    fn drop(&mut self) {
        // SAFETY: SHGetKnownFolderPath returns a CoTaskMem allocation, including on failure.
        unsafe {
            CoTaskMemFree(self.0.cast());
        }
    }
}

pub(super) fn system_parent() -> io::Result<PathBuf> {
    let mut folder = KnownFolder(null_mut());
    // SAFETY: GUID and output pointer are valid; null token selects the current user.
    let status =
        unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, null_mut(), &mut folder.0) };
    if status < 0 {
        return Err(io::Error::other(format!(
            "SHGetKnownFolderPath failed: {status:#x}"
        )));
    }
    if folder.0.is_null() {
        return Err(super::insecure());
    }
    // SAFETY: success returns an allocated, terminated UTF-16 string, retained by folder.
    let path = unsafe {
        let mut len = 0;
        while *folder.0.add(len) != 0 {
            len += 1;
        }
        OsString::from_wide(std::slice::from_raw_parts(folder.0, len))
    };
    Ok(PathBuf::from(path))
}

pub(super) fn name() -> &'static str {
    "hrdr-ipc-v1"
}

// usize storage provides TOKEN_USER's pointer alignment; the allocation never moves
// while its embedded SID pointer is used.
struct User(Vec<usize>);
impl User {
    fn current() -> io::Result<Self> {
        let mut raw = null_mut();
        // SAFETY: pseudo-handle and writable output are valid; no handle is inherited.
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut raw) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_NO_TOKEN as i32) {
                return Err(error);
            }
            // SAFETY: same output contract as OpenThreadToken.
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: successful token open transfers ownership of a real handle.
        let token = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut bytes = 0;
        // SAFETY: a null buffer with zero size requests the required allocation size.
        let result = unsafe {
            GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut bytes)
        };
        if result != 0
            || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
            || (bytes as usize) < size_of::<TOKEN_USER>()
        {
            return Err(super::insecure());
        }
        let mut data = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        // SAFETY: aligned buffer has at least bytes writable bytes, token remains live.
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                data.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(data))
    }

    fn sid(&self) -> PSID {
        // SAFETY: current populated the aligned buffer with a successful TOKEN_USER query.
        unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    fn sddl(&self) -> io::Result<String> {
        let mut string = null_mut();
        // SAFETY: SID is backed by self; the API allocates a terminated string.
        if unsafe { ConvertSidToStringSidW(self.sid(), &mut string) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let allocation = LocalAllocation(string.cast());
        // SAFETY: successful conversion returns a terminated UTF-16 allocation.
        let sid = unsafe {
            let mut len = 0;
            while *string.add(len) != 0 {
                len += 1;
            }
            String::from_utf16(std::slice::from_raw_parts(string, len)).map_err(io::Error::other)?
        };
        drop(allocation);
        Ok(format!("O:{sid}D:P(A;;FA;;;{sid})"))
    }
}

fn create(path: &Path, sddl: &str) -> io::Result<()> {
    let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = LocalAllocation(null_mut());
    // SAFETY: terminated input and valid output; revision 1 is SDDL_REVISION_1.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            1,
            &mut descriptor.0,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let path = wide(path)?;
    // SAFETY: both pointers remain valid for this synchronous call.
    if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
            return Err(error);
        }
    }
    Ok(())
}

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let mut text: Vec<u16> = path.as_os_str().encode_wide().collect();
    if text.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in IPC path",
        ));
    }
    text.push(0);
    Ok(text)
}

pub(super) fn open(parent: &Path) -> io::Result<File> {
    let user = User::current()?;
    let path = parent.join(name());
    create(&path, &user.sddl()?)?;
    let path = wide(&path)?;
    // SAFETY: terminated path; open the reparse point itself, never its target.
    let raw = unsafe {
        CreateFileW(
            path.as_ptr(),
            READ_CONTROL | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned an owned, valid handle.
    let file = unsafe { File::from_raw_handle(raw) };
    // SAFETY: this Win32 output struct permits zero initialization.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    // SAFETY: retained handle and writable output struct are valid.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
        != FILE_ATTRIBUTE_DIRECTORY
    {
        return Err(super::insecure());
    }
    validate(&file, &user)?;
    Ok(file)
}

fn validate(file: &File, user: &User) -> io::Result<()> {
    let mut descriptor = LocalAllocation(null_mut());
    let mut owner = null_mut();
    let mut acl: *mut ACL = null_mut();
    // SAFETY: outputs reference the returned descriptor allocation, kept alive below.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut acl,
            null_mut(),
            &mut descriptor.0,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: descriptor was returned successfully by GetSecurityInfo.
    if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: owner and ACL come from the OS-validated security descriptor. Nulls
    // are rejected before calling APIs that require valid SID/ACL pointers.
    if owner.is_null()
        || acl.is_null()
        || control & SE_DACL_PROTECTED == 0
        || unsafe { EqualSid(owner, user.sid()) } == 0
        || unsafe { IsValidAcl(acl) } == 0
    {
        return Err(super::insecure());
    }
    // Accept only the exact policy we create: a single explicit full-access user
    // grant. Object/callback/inherited/deny ACEs are rejected rather than interpreted.
    // SAFETY: valid ACL and ACE returned by the kernel; GetAce checks the index.
    unsafe {
        if (*acl).AceCount != 1 {
            return Err(super::insecure());
        }
        let mut ace = null_mut();
        if GetAce(acl, 0, &mut ace) == 0 {
            return Err(io::Error::last_os_error());
        }
        let header = &*ace.cast::<windows_sys::Win32::Security::ACE_HEADER>();
        // ACCESS_ALLOWED_ACE_TYPE is the fixed Win32 ABI value zero.
        if header.AceType != 0
            || header.AceFlags != 0
            || (header.AceSize as usize) < size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Err(super::insecure());
        }
        let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
        let sid = std::ptr::addr_of!(allowed.SidStart).cast_mut().cast();
        if allowed.Mask != FILE_ALL_ACCESS || EqualSid(sid, user.sid()) == 0 {
            return Err(super::insecure());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_ipc::UserDirectory;

    #[test]
    fn rejects_extra_grant_and_null_dacl() {
        let user = User::current().unwrap();
        for sddl in [
            format!("{}(A;;FA;;;WD)", user.sddl().unwrap()),
            "D:NO_ACCESS_CONTROL".into(),
        ] {
            let root = tempfile::tempdir().unwrap();
            create(&root.path().join(name()), &sddl).unwrap();
            assert!(
                UserDirectory::open_in(root.path()).is_err(),
                "accepted {sddl}"
            );
        }
    }

    #[test]
    fn rejects_inherited_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(name())).unwrap();
        assert!(UserDirectory::open_in(root.path()).is_err());
    }

    #[test]
    fn rejects_junction() {
        let root = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let _private_target = UserDirectory::open_in(target.path()).unwrap();
        let output = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(root.path().join(name()))
            .arg(target.path().join(name()))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(UserDirectory::open_in(root.path()).is_err());
    }
}

use super::{EndpointId, UserDirectory};
use std::{
    collections::HashSet,
    ffi::OsString,
    fs::File,
    io::{self, Write},
};

#[cfg(unix)]
#[path = "registration_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "registration_windows.rs"]
mod platform;

pub(super) use platform::{create, names, read};

#[cfg(test)]
thread_local! {
    pub(super) static PUBLICATION_FAILURE: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn inject_failure(stage: u8) -> io::Result<()> {
    assert!(
        stage != 1 || PUBLICATION_FAILURE.get() & 16 == 0,
        "injected publication panic"
    );
    if PUBLICATION_FAILURE.get() & stage != 0 {
        Err(io::Error::other(format!(
            "injected publication failure {stage}"
        )))
    } else {
        Ok(())
    }
}

const MUTATION_LOCK: &str = ".mutation.lock";

/// Closing the independently opened handle releases the kernel lock. Never unlink
/// or replace the lock file: cooperative writers must always lock the same object.
/// Other users are excluded by storage permissions; hostile same-user replacement
/// is outside the security boundary. Keep this guard out of asynchronous work.
pub(super) struct MutationGuard {
    _file: File,
}

struct UnwindTemporary<'a> {
    mutation: &'a MutationGuard,
    directory: &'a UserDirectory,
    endpoint: &'a EndpointId,
}

impl Drop for UnwindTemporary<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // There is no error return during unwinding. A failed cleanup remains
            // at the canonical path for the next guarded operation to recover.
            let _ = self
                .mutation
                .remove_temporary(self.directory, self.endpoint);
        }
    }
}

impl MutationGuard {
    pub(super) fn acquire(directory: &UserDirectory) -> io::Result<Self> {
        let file = match platform::read_write(directory, MUTATION_LOCK) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let snapshot = names(directory)?;
                if snapshot.overflow {
                    return Err(scan_limit());
                }
                require_room(snapshot.entries.len(), 1)?;
                match create(directory, MUTATION_LOCK) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        platform::read_write(directory, MUTATION_LOCK)?
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        file.try_lock().map_err(io::Error::from)?;
        Ok(Self { _file: file })
    }

    fn remove_temporary(&self, directory: &UserDirectory, endpoint: &EndpointId) -> io::Result<()> {
        let name = format!("{}.tmp", endpoint.name());
        if platform::validate_entry(directory, &name, false)? {
            #[cfg(test)]
            inject_failure(8)?;
            platform::remove(directory, &name)?;
        }
        Ok(())
    }

    pub(super) fn publish(
        &self,
        directory: &UserDirectory,
        endpoint: &EndpointId,
        bytes: &[u8],
    ) -> io::Result<()> {
        self.remove_temporary(directory, endpoint)?;
        let entries = self.reap(directory)?;
        // A rename consumes the temporary entry: first publication and replacement
        // both peak at one entry above this snapshot, not two.
        require_room(entries.len(), 1)?;
        let temporary = format!("{}.tmp", endpoint.name());
        let _unwind = UnwindTemporary {
            mutation: self,
            directory,
            endpoint,
        };
        let result = (|| {
            let mut file = create(directory, &temporary)?;
            #[cfg(test)]
            {
                file.write_all(&bytes[..bytes.len() / 2])?;
                inject_failure(1)?;
                file.write_all(&bytes[bytes.len() / 2..])?;
                inject_failure(2)?;
            }
            #[cfg(not(test))]
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            #[cfg(test)]
            inject_failure(4)?;
            platform::publish(self, directory, endpoint)
        })();
        if let Err(error) = result {
            // The closure closes the file before cleanup. After a successful rename,
            // postvalidation may fail; no rollback of the destination is promised.
            if let Err(cleanup) = self.remove_temporary(directory, endpoint) {
                return Err(io::Error::new(
                    error.kind(),
                    format!("{error}; cleanup of {temporary} failed: {cleanup}"),
                ));
            }
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn reap(&self, directory: &UserDirectory) -> io::Result<Vec<OsString>> {
        let snapshot = names(directory)?;
        let mut generations = HashSet::new();
        for name in &snapshot.entries {
            let Some(text) = name.to_str() else { continue };
            let stem = [".json", ".tmp", ".lease"]
                .into_iter()
                .find_map(|suffix| text.strip_suffix(suffix))
                .unwrap_or(text);
            if EndpointId::from_name(stem).is_none() || !generations.insert(stem) {
                continue;
            }
            let lease_name = format!("{stem}.lease");
            let lease = match platform::read_write(directory, &lease_name) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            match lease.try_lock().map_err(io::Error::from) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let endpoint = EndpointId::from_name(stem).expect("validated generation");
                    self.remove_temporary(directory, &endpoint)?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            let associated = [
                stem.to_owned(),
                format!("{stem}.json"),
                format!("{stem}.tmp"),
            ];
            let mut present = Vec::new();
            for (index, name) in associated.iter().enumerate() {
                if platform::validate_entry(directory, name, index == 0)? {
                    present.push(name);
                }
            }
            for name in present {
                platform::remove(directory, name)?;
            }
            // Keep the locked handle until all associated entries have been removed.
            platform::remove(directory, &lease_name)?;
        }
        let remaining = names(directory)?;
        if remaining.overflow {
            return Err(scan_limit());
        }
        Ok(remaining.entries)
    }
}

pub(super) struct Snapshot {
    entries: Vec<OsString>,
    overflow: bool,
}

impl Snapshot {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            overflow: false,
        }
    }
}

fn push_name(names: &mut Snapshot, name: OsString) -> bool {
    if names.entries.len() == super::MAX_DIRECTORY_ENTRIES {
        names.overflow = true;
        return false;
    }
    names.entries.push(name);
    true
}

pub(super) fn require_room(entries: usize, additional: usize) -> io::Result<()> {
    if entries + additional > super::MAX_DIRECTORY_ENTRIES {
        return Err(scan_limit());
    }
    Ok(())
}

fn scan_limit() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "IPC directory scan limit exceeded",
    )
}

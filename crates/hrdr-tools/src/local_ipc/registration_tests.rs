use super::*;
use std::{io::Write, path::Path, time::Duration};

#[tokio::test]
async fn publication_failures_preserve_record_and_recover_temporary() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let listener = Listener::bind(&directory).unwrap();
    let old = descriptor(&listener);
    let mut registration = Registration::publish(&listener, old.clone()).unwrap();
    let record = directory.path.join(record_name(listener.endpoint()));
    let temporary = directory
        .path
        .join(format!("{}.tmp", listener.endpoint().name()));
    let bytes = std::fs::read(&record).unwrap();
    let metadata = std::fs::metadata(&record).unwrap();
    let count = std::fs::read_dir(&directory.path).unwrap().count();
    let mut new = old.clone();
    new.session_name = "replacement".into();
    for stage in [1, 2, 4] {
        for _ in 0..8 {
            storage::PUBLICATION_FAILURE.set(stage);
            let error = registration.refresh(&listener, new.clone()).unwrap_err();
            storage::PUBLICATION_FAILURE.set(0);
            assert_eq!(
                error.to_string(),
                format!("injected publication failure {stage}")
            );
            assert_eq!(registration.descriptor(), &old);
            assert_eq!(std::fs::read(&record).unwrap(), bytes);
            let after = std::fs::metadata(&record).unwrap();
            assert_eq!(after.len(), metadata.len());
            assert_eq!(after.modified().unwrap(), metadata.modified().unwrap());
            assert_eq!(after.created().ok(), metadata.created().ok());
            assert_eq!(after.permissions(), metadata.permissions());
            assert!(!temporary.exists());
            assert_eq!(std::fs::read_dir(&directory.path).unwrap().count(), count);
        }
    }
    for recover_by_listing in [true, false] {
        storage::PUBLICATION_FAILURE.set(1 | 8);
        let error = registration.refresh(&listener, new.clone()).unwrap_err();
        storage::PUBLICATION_FAILURE.set(0);
        let message = error.to_string();
        assert!(message.contains("injected publication failure 1"));
        assert!(message.contains("cleanup of"));
        assert!(message.contains("injected publication failure 8"));
        assert_eq!(registration.descriptor(), &old);
        assert_eq!(std::fs::read(&record).unwrap(), bytes);
        assert_eq!(
            std::fs::read(&temporary).unwrap(),
            serde_json::to_vec(&new).unwrap()[..serde_json::to_vec(&new).unwrap().len() / 2]
        );
        assert_eq!(
            std::fs::read_dir(&directory.path).unwrap().count(),
            count + 1
        );
        if recover_by_listing {
            assert_eq!(directory.list_candidates().unwrap(), vec![old.clone()]);
        } else {
            registration.refresh(&listener, old.clone()).unwrap();
        }
        assert!(!temporary.exists());
        assert_eq!(std::fs::read_dir(&directory.path).unwrap().count(), count);
    }
    storage::PUBLICATION_FAILURE.set(16);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        registration.refresh(&listener, new.clone()).unwrap();
    }));
    storage::PUBLICATION_FAILURE.set(0);
    assert!(panic.is_err());
    assert!(!temporary.exists());
    assert_eq!(registration.descriptor(), &old);
    assert_eq!(std::fs::read(&record).unwrap(), bytes);
    registration.refresh(&listener, new.clone()).unwrap();
    assert_eq!(directory.list_candidates().unwrap(), vec![new]);
}

async fn retry_busy<T>(mut operation: impl FnMut() -> io::Result<T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match operation() {
                Ok(value) => break value,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(error) => panic!("unexpected operation failure: {error}"),
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_refresh_and_independent_listing_are_exact_snapshots() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let reader = UserDirectory::open_in(root.path()).unwrap();
    let listener = Listener::bind(&directory).unwrap();
    let old = descriptor(&listener);
    let mut new = old.clone();
    new.session_id = "replacement identity".into();
    new.session_name = "replacement name".into();
    new.working_directory = "replacement directory".into();
    let mut registration = Registration::publish(&listener, old.clone()).unwrap();
    let writer_old = old.clone();
    let writer_new = new.clone();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let writer_barrier = barrier.clone();
    let writer = tokio::spawn(async move {
        writer_barrier.wait().await;
        for index in 0..128 {
            let value = if index % 2 == 0 {
                &writer_new
            } else {
                &writer_old
            };
            retry_busy(|| registration.refresh(&listener, value.clone())).await;
            tokio::task::yield_now().await;
        }
        (listener, registration)
    });
    barrier.wait().await;
    for _ in 0..128 {
        let snapshot = retry_busy(|| reader.list_candidates()).await;
        assert!(
            snapshot == vec![old.clone()] || snapshot == vec![new.clone()],
            "{snapshot:?}"
        );
        tokio::task::yield_now().await;
    }
    let (_listener, registration) = writer.await.unwrap();
    assert_eq!(registration.descriptor(), &old);
    assert_eq!(reader.list_candidates().unwrap(), vec![old]);
}

fn root() -> tempfile::TempDir {
    #[cfg(unix)]
    let root = tempfile::tempdir_in("/tmp");
    #[cfg(windows)]
    let root = tempfile::tempdir();
    root.unwrap()
}

fn descriptor(listener: &Listener) -> SessionDescriptor {
    SessionDescriptor {
        endpoint: listener.endpoint().clone(),
        session_id: "session-identity".into(),
        session_name: "duplicate name".into(),
        working_directory: "working directory".into(),
    }
}

#[tokio::test]
async fn mutation_lock_blocks_independent_handles_and_registration() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let reopened = UserDirectory::open_in(root.path()).unwrap();
    let listener = Listener::bind(&directory).unwrap();
    let mut registration = Registration::publish(&listener, descriptor(&listener)).unwrap();
    let before = registration.descriptor().clone();
    let path = directory.path.join(record_name(listener.endpoint()));
    let bytes = std::fs::read(&path).unwrap();
    let guard = storage::MutationGuard::acquire(&directory).unwrap();
    assert_eq!(
        storage::MutationGuard::acquire(&reopened)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(
        reopened.list_candidates().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    let mut changed = before.clone();
    changed.session_name = "changed".into();
    assert_eq!(
        registration
            .refresh(&listener, changed.clone())
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(registration.descriptor(), &before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    drop(guard);
    assert!(directory.path.join(".mutation.lock").is_file());
    registration.refresh(&listener, changed.clone()).unwrap();
    assert_eq!(reopened.list_candidates().unwrap(), vec![changed]);
}

#[test]
fn mutation_lock_rejects_wrong_type() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let path = directory.path.join(".mutation.lock");
    std::fs::create_dir(&path).unwrap();
    assert!(storage::MutationGuard::acquire(&directory).is_err());
    assert!(path.is_dir());
}

#[tokio::test]
async fn native_mutation_lock_child() {
    let Some(root) = std::env::var_os("HRDR_MUTATION_CHILD_ROOT") else {
        return;
    };
    let directory = UserDirectory::open_in(Path::new(&root)).unwrap();
    let _guard = storage::MutationGuard::acquire(&directory).unwrap();
    println!("MUTATION_READY");
    std::io::stdout().flush().unwrap();
    let mut release = String::new();
    std::io::stdin().read_line(&mut release).unwrap();
    assert_eq!(release, "release\n");
}

#[tokio::test]
async fn mutation_lock_child_readiness_and_release() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "local_ipc::registration::tests::native_mutation_lock_child",
            "--nocapture",
        ])
        .env("HRDR_MUTATION_CHILD_ROOT", root.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("child exited before readiness");
            if line == "MUTATION_READY" {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        storage::MutationGuard::acquire(&directory)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"release\n")
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(30), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let _guard = storage::MutationGuard::acquire(&directory).unwrap();
    assert!(directory.path.join(".mutation.lock").is_file());
}

#[tokio::test]
async fn duplicate_names_refresh_and_stale_candidates() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let first = Listener::bind(&directory).unwrap();
    let second = Listener::bind(&directory).unwrap();
    let mut first_record = Registration::publish(&first, descriptor(&first)).unwrap();
    let second_record = Registration::publish(&second, descriptor(&second)).unwrap();
    let mut refreshed = first_record.descriptor().clone();
    refreshed.session_name = "renamed".into();
    refreshed.session_id = "new session".into();
    refreshed.working_directory = "new directory".into();
    first_record.refresh(&first, refreshed.clone()).unwrap();
    assert_eq!(first_record.descriptor(), &refreshed);
    let candidates = directory.list_candidates().unwrap();
    assert_eq!(candidates.len(), 2);
    assert!(candidates.contains(&refreshed));
    assert!(candidates.contains(second_record.descriptor()));
    assert!(Registration::publish(&first, descriptor(&second)).is_err());
    assert!(first_record.refresh(&second, descriptor(&second)).is_err());
    let temporary = format!("{}.tmp", refreshed.endpoint.name());
    storage::create(&directory, &temporary).unwrap();
    drop(first);
    assert!(!directory.list_candidates().unwrap().contains(&refreshed));
    assert!(!directory.path.join(temporary).exists());
    assert_eq!(
        directory.list_candidates().unwrap(),
        vec![descriptor(&second)]
    );
    assert!(
        !directory
            .path
            .join(record_name(&refreshed.endpoint))
            .exists()
    );
    assert!(
        !directory
            .path
            .join(format!("{}.lease", refreshed.endpoint.name()))
            .exists()
    );
    // Dropping stale registration handles never removes a replacement.
    let replacement = descriptor(&second);
    let path = directory.path.join(record_name(&refreshed.endpoint));
    std::fs::write(&path, serde_json::to_vec(&replacement).unwrap()).unwrap();
    drop(first_record);
    drop(second_record);
    assert_eq!(
        std::fs::read(path).unwrap(),
        serde_json::to_vec(&replacement).unwrap()
    );
}

#[tokio::test]
async fn rejects_malicious_records_and_bounds() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let listener = Listener::bind(&directory).unwrap();
    let expected = descriptor(&listener);
    let record = Registration::publish(&listener, expected.clone()).unwrap();
    let path = directory.path.join(record_name(listener.endpoint()));
    let original = serde_json::to_value(&expected).unwrap();
    for (field, max) in [
        ("session_id", MAX_SESSION_ID_BYTES),
        ("session_name", MAX_SESSION_NAME_BYTES),
        ("working_directory", MAX_WORKING_DIRECTORY_BYTES),
    ] {
        let mut boundary = original.clone();
        boundary[field] = "x".repeat(max).into();
        assert!(serde_json::from_value::<SessionDescriptor>(boundary).is_ok());
        for value in [String::new(), "x".repeat(max + 1), "nul\0value".into()] {
            let mut bad = original.clone();
            bad[field] = value.into();
            assert!(serde_json::from_value::<SessionDescriptor>(bad.clone()).is_err());
            std::fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
            assert!(directory.list_candidates().is_err());
        }
    }
    for bad in [
        {
            let mut bad = original.clone();
            bad["version"] = 2.into();
            serde_json::to_vec(&bad).unwrap()
        },
        {
            let mut bad = original.clone();
            bad["secret"] = "unexpected".into();
            serde_json::to_vec(&bad).unwrap()
        },
        {
            let mut bad = original.clone();
            bad["endpoint"]["pid"] = 0.into();
            serde_json::to_vec(&bad).unwrap()
        },
        b"{\"version\":1,".to_vec(),
        {
            let mut bytes = serde_json::to_vec(&expected).unwrap();
            bytes.resize(MAX_RECORD_BYTES + 1, b' ');
            bytes
        },
        serde_json::to_vec(&SessionDescriptor {
            endpoint: EndpointId::fresh(),
            ..expected.clone()
        })
        .unwrap(),
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(directory.list_candidates().is_err());
    }
    Registration::publish(&listener, expected.clone()).unwrap();
    assert_eq!(directory.list_candidates().unwrap(), vec![expected]);
    let mut invalid = record.descriptor().clone();
    invalid.session_id = "x".repeat(MAX_SESSION_ID_BYTES + 1);
    assert!(Registration::publish(&listener, invalid).is_err());
    std::fs::write(directory.path.join("invalid.json"), b"{}").unwrap();
    assert!(directory.list_candidates().is_err());
}

#[tokio::test]
async fn bind_retries_after_observed_mutation_contention() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let guard = storage::MutationGuard::acquire(&directory).unwrap();
    assert_eq!(
        Listener::bind(&directory).err().unwrap().kind(),
        io::ErrorKind::WouldBlock
    );
    let (contended, observed) = tokio::sync::oneshot::channel();
    let mut contended = Some(contended);
    let bind = async {
        let operation = || {
            let result = Listener::bind(&directory);
            if let Some(contended) = contended.take() {
                assert_eq!(
                    result.as_ref().err().unwrap().kind(),
                    io::ErrorKind::WouldBlock
                );
                contended.send(()).unwrap();
            }
            result
        };
        retry_busy(operation).await
    };
    let release = async {
        observed.await.unwrap();
        drop(guard);
    };
    let (listener, ()) = tokio::join!(bind, release);
    let expected = descriptor(&listener);
    let _record = Registration::publish(&listener, expected.clone()).unwrap();
    assert_eq!(directory.list_candidates().unwrap(), vec![expected]);
}

#[tokio::test]
async fn socket_only_turnover_stays_bounded_with_live_survivor() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let mut survivor = retry_busy(|| Listener::bind(&directory)).await;
    let record = Registration::publish(&survivor, descriptor(&survivor)).unwrap();
    for _ in 0..=MAX_DIRECTORY_ENTRIES {
        let listener = retry_busy(|| Listener::bind(&directory)).await;
        assert_eq!(listener.endpoint().pid(), survivor.endpoint().pid());
        drop(listener);
        assert!(std::fs::read_dir(&directory.path).unwrap().count() <= 6);
    }
    assert_eq!(
        directory.list_candidates().unwrap(),
        vec![record.descriptor().clone()]
    );
    let expected = 3 + usize::from(cfg!(unix));
    assert_eq!(
        std::fs::read_dir(&directory.path).unwrap().count(),
        expected
    );
    let guard = storage::MutationGuard::acquire(&directory).unwrap();
    assert_eq!(
        Listener::bind(&directory).err().unwrap().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(guard);
    let deadline = Duration::from_secs(5);
    let mut client =
        crate::local_ipc::Connection::connect(&directory, survivor.endpoint(), deadline)
            .await
            .unwrap();
    let mut accepted = survivor.accept(deadline).await.unwrap();
    drop(survivor);
    assert!(directory.list_candidates().unwrap().is_empty());
    assert_eq!(std::fs::read_dir(&directory.path).unwrap().count(), 1);
    client
        .write_frame(b"still connected", deadline)
        .await
        .unwrap();
    assert_eq!(
        accepted.read_frame(deadline).await.unwrap(),
        b"still connected"
    );
}

#[tokio::test]
async fn reaper_preserves_insecure_generation_and_legacy() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let listener = Listener::bind(&directory).unwrap();
    let endpoint = listener.endpoint().clone();
    let record = record_name(&endpoint);
    drop(listener);
    std::fs::create_dir(directory.path.join(&record)).unwrap();
    assert!(directory.list_candidates().is_err());
    assert!(directory.path.join(&record).is_dir());
    assert!(
        directory
            .path
            .join(format!("{}.lease", endpoint.name()))
            .is_file()
    );
    #[cfg(unix)]
    assert!(directory.path.join(endpoint.name()).exists());
    std::fs::rename(
        directory.path.join(&record),
        directory.path.join("saved-directory"),
    )
    .unwrap();
    let legacy = format!("{}.tmp", EndpointId::fresh().name());
    storage::create(&directory, &legacy).unwrap();
    assert!(directory.list_candidates().unwrap().is_empty());
    assert!(directory.path.join(legacy).is_file());
    assert!(directory.path.join("saved-directory").is_dir());
}

#[tokio::test]
async fn reaper_refuses_invalid_lease_and_temporary_paths() {
    for suffix in ["lease", "tmp"] {
        let root = root();
        let directory = UserDirectory::open_in(root.path()).unwrap();
        let listener = Listener::bind(&directory).unwrap();
        let name = format!("{}.{suffix}", listener.endpoint().name());
        drop(listener);
        let path = directory.path.join(&name);
        if suffix == "lease" {
            std::fs::rename(&path, directory.path.join("saved-lease")).unwrap();
        }
        std::fs::create_dir(&path).unwrap();
        let before = std::fs::read_dir(&directory.path).unwrap().count();
        assert!(directory.list_candidates().is_err());
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(&directory.path).unwrap().count(), before);
    }
}

#[test]
fn overflow_snapshot_reclaims_encountered_generations() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let guard = storage::MutationGuard::acquire(&directory).unwrap();
    for _ in 0..MAX_DIRECTORY_ENTRIES {
        storage::create(&directory, &format!("{}.lease", EndpointId::fresh().name())).unwrap();
    }
    assert!(guard.reap(&directory).unwrap().len() <= 2);
    assert_eq!(guard.reap(&directory).unwrap().len(), 1);
}

#[tokio::test]
async fn creations_respect_total_entry_cap() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let listener = Listener::bind(&directory).unwrap();
    let count = std::fs::read_dir(&directory.path).unwrap().count();
    for index in count..MAX_DIRECTORY_ENTRIES {
        std::fs::write(directory.path.join(format!("unrelated-{index}")), b"").unwrap();
    }
    assert_eq!(
        Listener::bind(&directory).err().unwrap().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(
        Registration::publish(&listener, descriptor(&listener))
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(
        std::fs::read_dir(&directory.path).unwrap().count(),
        MAX_DIRECTORY_ENTRIES
    );
}

#[tokio::test]
async fn publication_reserves_only_peak_additions() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let listener = Listener::bind(&directory).unwrap();
    let count = std::fs::read_dir(&directory.path).unwrap().count();
    for index in count..MAX_DIRECTORY_ENTRIES - 1 {
        std::fs::write(directory.path.join(format!("filler-{index}")), b"").unwrap();
    }
    let old = descriptor(&listener);
    let mut registration = Registration::publish(&listener, old.clone()).unwrap();
    assert_eq!(
        std::fs::read_dir(&directory.path).unwrap().count(),
        MAX_DIRECTORY_ENTRIES
    );
    let mut new = old.clone();
    new.session_name = "at capacity".into();
    assert_eq!(
        registration
            .refresh(&listener, new.clone())
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(registration.descriptor(), &old);
    assert_eq!(directory.list_candidates().unwrap(), vec![old]);
    std::fs::rename(
        directory.path.join(format!("filler-{count}")),
        root.path().join("saved-filler"),
    )
    .unwrap();
    registration.refresh(&listener, new.clone()).unwrap();
    assert_eq!(
        std::fs::read_dir(&directory.path).unwrap().count(),
        MAX_DIRECTORY_ENTRIES - 1
    );
    assert_eq!(directory.list_candidates().unwrap(), vec![new]);
}

#[test]
fn scanner_counts_unrelated_entries() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    for index in 0..MAX_DIRECTORY_ENTRIES {
        std::fs::write(directory.path.join(format!("unrelated-{index}")), b"").unwrap();
    }
    assert_eq!(
        directory.list_candidates().unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert!(!directory.path.join(".mutation.lock").exists());
    std::fs::rename(
        directory.path.join("unrelated-0"),
        root.path().join("saved-entry"),
    )
    .unwrap();
    // The permanent mutation lock itself consumes one scan slot.
    assert!(directory.list_candidates().unwrap().is_empty());
    std::fs::write(directory.path.join("one-too-many"), b"").unwrap();
    assert_eq!(
        directory.list_candidates().unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[tokio::test]
async fn native_registration_child() {
    let Some(root) = std::env::var_os("HRDR_REGISTRATION_CHILD_ROOT") else {
        return;
    };
    let directory = UserDirectory::open_in(Path::new(&root)).unwrap();
    let parent: SessionDescriptor =
        serde_json::from_str(&std::env::var("HRDR_REGISTRATION_PARENT").unwrap()).unwrap();
    assert_eq!(directory.list_candidates().unwrap(), vec![parent]);
    let listener = Listener::bind(&directory).unwrap();
    let child = descriptor(&listener);
    Registration::publish(&listener, child.clone()).unwrap();
    assert!(directory.list_candidates().unwrap().contains(&child));
    println!("REGISTRATION_READY");
    std::io::stdout().flush().unwrap();
    let mut release = String::new();
    std::io::stdin().read_line(&mut release).unwrap();
    assert_eq!(release, "release\n");
}

#[tokio::test]
async fn native_cross_process_publication_and_read() {
    child_lease_release(false).await;
}

#[tokio::test]
async fn killed_child_releases_generation() {
    child_lease_release(true).await;
}

async fn child_lease_release(kill: bool) {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let listener = Listener::bind(&directory).unwrap();
    let parent = descriptor(&listener);
    Registration::publish(&listener, parent.clone()).unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "local_ipc::registration::tests::native_registration_child",
            "--nocapture",
        ])
        .env("HRDR_REGISTRATION_CHILD_ROOT", root.path())
        .env(
            "HRDR_REGISTRATION_PARENT",
            serde_json::to_string(&parent).unwrap(),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let child_pid = child.id().unwrap();
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(30), async {
        while lines
            .next_line()
            .await
            .unwrap()
            .expect("child exited before readiness")
            != "REGISTRATION_READY"
        {}
    })
    .await
    .unwrap();
    let candidates = directory.list_candidates().unwrap();
    assert_eq!(candidates.len(), 2);
    assert!(candidates.contains(&parent));
    let candidate = candidates
        .iter()
        .find(|candidate| candidate.endpoint.pid() == child_pid)
        .unwrap();
    assert_eq!(candidate.session_name, parent.session_name);
    assert_eq!(candidate.session_id, parent.session_id);
    assert_eq!(candidate.working_directory, parent.working_directory);
    assert_ne!(candidate.endpoint, parent.endpoint);
    if kill {
        child.start_kill().unwrap();
    } else {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"release\n")
            .await
            .unwrap();
    }
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.success(), !kill);
    assert_eq!(directory.list_candidates().unwrap(), vec![parent]);
    assert!(
        !directory
            .path
            .join(record_name(&candidate.endpoint))
            .exists()
    );
    assert!(
        !directory
            .path
            .join(format!("{}.lease", candidate.endpoint.name()))
            .exists()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unix_rejects_symlinks_modes_and_nonregular_files() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let listener = Listener::bind(&directory).unwrap();
    Registration::publish(&listener, descriptor(&listener)).unwrap();
    let path = directory.path.join(record_name(listener.endpoint()));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    drop(listener);
    assert!(directory.list_candidates().is_err());
    std::fs::rename(&path, directory.path.join("saved")).unwrap();
    symlink(directory.path.join("saved"), &path).unwrap();
    assert!(directory.list_candidates().is_err());
    std::fs::rename(&path, directory.path.join("saved-link")).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(directory.list_candidates().is_err());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_rejects_extended_file_acl() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let listener = Listener::bind(&directory).unwrap();
    Registration::publish(&listener, descriptor(&listener)).unwrap();
    let path = directory.path.join(record_name(listener.endpoint()));
    let output = std::process::Command::new("/bin/chmod")
        .arg("+a")
        .arg("everyone allow read")
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(directory.list_candidates().is_err());
}

#[cfg(windows)]
#[tokio::test]
async fn windows_rejects_unprotected_files_and_reparse_points() {
    let root = root();
    let directory = UserDirectory::open_in(root.path()).unwrap();
    let listener = Listener::bind(&directory).unwrap();
    let path = directory.path.join(record_name(listener.endpoint()));
    std::fs::write(&path, serde_json::to_vec(&descriptor(&listener)).unwrap()).unwrap();
    drop(listener);
    assert!(directory.list_candidates().is_err());
    std::fs::rename(&path, directory.path.join("saved")).unwrap();
    let target = tempfile::tempdir().unwrap();
    let output = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&path)
        .arg(target.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(directory.list_candidates().is_err());
}

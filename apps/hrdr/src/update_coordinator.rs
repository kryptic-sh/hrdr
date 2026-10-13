//! One-shot hidden self-update coordinator.
//!
//! The request is claimed by rename before it is parsed. Windows waits for the
//! exact old process instance to exit, installs the admitted checkout into the
//! default Cargo bin, attests that installed binary, and starts its handoff
//! recovery. Post-exit failures recover once through the retained old binary.

#[cfg(windows)]
use crate::update_check::AdmittedCheckout;
use sha2::{Digest, Sha256};
#[cfg(windows)]
use std::process::Command;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

const SCHEMA_VERSION: u32 = 5;
const MAX_RETRIES: u8 = 3;
const MAX_DEADLINE_SECS: u64 = 24 * 60 * 60;
#[cfg(all(windows, test))]
const FILE_SHARE_READ: u32 = 0x0000_0001;

#[derive(Debug)]
struct Request {
    session_id: String,
    checkout_root: PathBuf,
    recovery_cwd: PathBuf,
    expected_commit: String,
    expected_version: String,
    candidate_binary: PathBuf,
    #[cfg(all(windows, test))]
    prebuilt_binary: PathBuf,
    #[cfg(all(windows, test))]
    prebuilt_checksum: String,
    intended_cargo_bin: PathBuf,
    expected_target: String,
    operation_id: String,
    source_pid: u32,
    source_created: u64,
    retry_limit: u8,
    deadline_secs: u64,
    checksum: String,
}

/// A Windows coordinator request fully prepared before a TUI handoff commits.
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) struct PreparedWindowsUpdateCoordinator {
    candidate_binary: PathBuf,
    request_path: PathBuf,
}

#[cfg(windows)]
#[allow(dead_code)]
impl PreparedWindowsUpdateCoordinator {
    pub(crate) fn prepare(
        session_id: &str,
        operation_id: &hrdr_agent::UpdateOperationId,
        checkout: &AdmittedCheckout,
        recovery_cwd: &Path,
    ) -> anyhow::Result<Self> {
        use anyhow::Context;
        use std::io::Write;

        let checkout_root = canonical_absolute(checkout.canonical_root()).map_err(|_| {
            anyhow::anyhow!("update checkout root is not a canonical absolute path")
        })?;
        let expected_commit = checkout.expected_commit();
        anyhow::ensure!(
            checkout_root.is_dir(),
            "update checkout root is not a directory"
        );
        // The saved session is keyed by the exact cwd string it was saved under,
        // so recovery must start in that same spelling: canonicalizing would turn
        // an 8.3 short name or a junction into a different session directory.
        // The canonical form is only used to check containment.
        let canonical_recovery_cwd = canonical_absolute(recovery_cwd)
            .map_err(|_| anyhow::anyhow!("recovery cwd is not a canonical absolute path"))?;
        anyhow::ensure!(
            canonical_recovery_cwd.starts_with(&checkout_root),
            "recovery cwd escapes the admitted checkout"
        );
        let recovery_cwd = recovery_cwd.to_path_buf();
        anyhow::ensure!(valid_text(session_id, 128), "update session id is invalid");
        anyhow::ensure!(is_full_commit(expected_commit), "update commit is invalid");
        anyhow::ensure!(
            valid_text(operation_id.as_str(), 128),
            "update operation id is invalid"
        );
        let source = hrdr_tools::current_process_lease()
            .ok_or_else(|| anyhow::anyhow!("current process lease is unavailable"))?;
        let intended_cargo_bin = canonical_absolute(
            &std::env::current_exe().context("locating the current executable")?,
        )
        .map_err(|_| anyhow::anyhow!("current executable is not a canonical absolute file"))?;
        anyhow::ensure!(
            intended_cargo_bin == default_cargo_bin()?,
            "current executable is not the default Cargo install"
        );
        anyhow::ensure!(
            intended_cargo_bin.is_file(),
            "current executable is not a regular file"
        );

        let operation_hash = sha256_hex(operation_id.as_str().as_bytes());
        let executable_parent = intended_cargo_bin
            .parent()
            .ok_or_else(|| anyhow::anyhow!("current executable has no parent directory"))?;
        let operation_root = executable_parent.join(format!(".hrdr-update-{operation_hash}"));
        fs::create_dir(&operation_root).context("creating update operation directory")?;
        let operation_root = canonical_absolute(&operation_root)
            .map_err(|_| anyhow::anyhow!("update operation directory is not canonical"))?;
        anyhow::ensure!(
            operation_root.parent() == Some(executable_parent),
            "update operation directory escapes the executable directory"
        );
        let expected_version = env!("CARGO_PKG_VERSION");
        let expected_target = env!("HRDR_BUILD_TARGET");

        let extension = intended_cargo_bin
            .extension()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("exe");
        let candidate_binary =
            operation_root.join(format!("coordinator-{operation_hash}.{extension}"));
        let source_bytes =
            fs::read(&intended_cargo_bin).context("reading the current executable")?;
        let mut candidate = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate_binary)
            .context("staging the current executable")?;
        candidate
            .write_all(&source_bytes)
            .context("writing the staged coordinator executable")?;
        candidate
            .sync_all()
            .context("flushing the staged coordinator executable")?;
        drop(candidate);
        let candidate_binary = canonical_absolute(&candidate_binary)
            .map_err(|_| anyhow::anyhow!("staged coordinator executable is not canonical"))?;

        let request_path = operation_root.join(format!("request-{operation_hash}.json"));
        let mut request = Request {
            session_id: session_id.to_owned(),
            checkout_root,
            recovery_cwd,
            expected_commit: expected_commit.to_owned(),
            expected_version: expected_version.to_owned(),
            candidate_binary: candidate_binary.clone(),
            #[cfg(all(windows, test))]
            prebuilt_binary: PathBuf::new(),
            #[cfg(all(windows, test))]
            prebuilt_checksum: String::new(),
            intended_cargo_bin,
            expected_target: expected_target.to_owned(),
            operation_id: operation_id.as_str().to_owned(),
            source_pid: source.pid,
            source_created: source.created,
            retry_limit: MAX_RETRIES,
            deadline_secs: MAX_DEADLINE_SECS,
            checksum: String::new(),
        };
        request.checksum = sha256_hex(canonical_body(&request).as_bytes());
        let bytes = serialized_request(&request);
        let parsed = parse_request(&bytes)
            .map_err(|_| anyhow::anyhow!("generated update coordinator request is invalid"))?;
        validate_request_path(&request_path, &parsed).map_err(|_| {
            anyhow::anyhow!("generated update coordinator request has invalid paths")
        })?;
        let mut request_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&request_path)
            .context("creating update coordinator request")?;
        request_file
            .write_all(&bytes)
            .context("writing update coordinator request")?;
        request_file
            .sync_all()
            .context("flushing update coordinator request")?;
        drop(request_file);
        let request_path = canonical_absolute(&request_path)
            .map_err(|_| anyhow::anyhow!("update coordinator request is not canonical"))?;

        Ok(Self {
            candidate_binary,
            request_path,
        })
    }
}

#[cfg(windows)]
impl hrdr_tui::PreparedUpdateCoordinator for PreparedWindowsUpdateCoordinator {
    fn launch(self: Box<Self>) -> anyhow::Result<()> {
        use anyhow::Context;

        Command::new(&self.candidate_binary)
            .args([
                std::ffi::OsStr::new("__update-coordinator"),
                self.request_path.as_os_str(),
            ])
            .spawn()
            .context("spawning the prepared update coordinator")?;
        Ok(())
    }
}

/// Dispatch the hidden argument before clap, configuration, sessions, or the TUI.
pub fn run_from_args() -> Option<anyhow::Result<()>> {
    let mut args = std::env::args_os();
    let _program = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new("__update-coordinator")) {
        return None;
    }

    let result = match (args.next(), args.next()) {
        (Some(path), None) => run(PathBuf::from(path)),
        _ => Status::Invalid,
    };
    println!(
        "{{\"schema_version\":{SCHEMA_VERSION},\"status\":\"{}\"}}",
        result.as_str()
    );
    Some(Ok(()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Invalid,
    Reused,
    #[cfg(windows)]
    Stale,
    #[cfg(windows)]
    WaitTimeout,
    #[cfg(windows)]
    SourceUnknown,
    #[cfg(windows)]
    InstallFailed,
    #[cfg(windows)]
    IdentityFailed,
    #[cfg(windows)]
    LaunchFailed,
    #[cfg(windows)]
    OperationMarkerError,
    #[cfg(windows)]
    Launched,
    #[cfg(not(windows))]
    Unsupported,
}

impl Status {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Reused => "reused",
            #[cfg(windows)]
            Self::Stale => "stale",
            #[cfg(windows)]
            Self::WaitTimeout => "wait_timeout",
            #[cfg(windows)]
            Self::SourceUnknown => "source_unknown",
            #[cfg(windows)]
            Self::InstallFailed => "install_failed",
            #[cfg(windows)]
            Self::IdentityFailed => "identity_failed",
            #[cfg(windows)]
            Self::LaunchFailed => "launch_failed",
            #[cfg(windows)]
            Self::OperationMarkerError => "operation_marker_error",
            #[cfg(windows)]
            Self::Launched => "launched",
            #[cfg(not(windows))]
            Self::Unsupported => "unsupported",
        }
    }
}

fn run(request_path: PathBuf) -> Status {
    let Ok(in_progress) = claim(&request_path) else {
        return Status::Reused;
    };
    let Ok(bytes) = fs::read(&in_progress) else {
        return Status::Invalid;
    };
    let Ok(request) = parse_request(&bytes) else {
        return Status::Invalid;
    };
    let Ok(checkout_root) = validate_request_path(&in_progress, &request) else {
        return Status::Invalid;
    };

    #[cfg(windows)]
    {
        run_windows(
            &request,
            &checkout_root,
            in_progress.parent().expect("claimed path has parent"),
        )
    }
    #[cfg(not(windows))]
    {
        let _ = (request, checkout_root);
        Status::Unsupported
    }
}

#[cfg(windows)]
fn run_windows(request: &Request, checkout_root: &Path, staging_root: &Path) -> Status {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(request.deadline_secs);
    let lease = hrdr_tools::ProcessLease {
        pid: request.source_pid,
        created: request.source_created,
    };
    loop {
        match hrdr_tools::process_lease_liveness(lease) {
            hrdr_tools::ProcessLiveness::Dead | hrdr_tools::ProcessLiveness::Replaced => break,
            hrdr_tools::ProcessLiveness::Unknown => return Status::SourceUnknown,
            hrdr_tools::ProcessLiveness::Live if std::time::Instant::now() >= deadline => {
                return Status::WaitTimeout;
            }
            hrdr_tools::ProcessLiveness::Live => {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
        }
    }

    let recover = |failure: Status| {
        if launch(
            &request.candidate_binary,
            &request.session_id,
            &request.recovery_cwd,
            Some(failure.as_str()),
            lease,
        ) {
            failure
        } else {
            Status::LaunchFailed
        }
    };
    if !clean_head(checkout_root, &request.expected_commit, deadline) {
        return recover(Status::Stale);
    }
    if !install_admitted_checkout(checkout_root, request, deadline) {
        return recover(Status::InstallFailed);
    }
    if !prebuilt_identity(
        &request.intended_cargo_bin,
        &request.expected_version,
        &request.expected_commit,
        &request.expected_target,
        deadline,
    ) {
        return recover(Status::IdentityFailed);
    }
    match consume_operation(staging_root, request) {
        OperationMarker::Created => {}
        OperationMarker::Existing => return Status::Reused,
        OperationMarker::Error => return recover(Status::OperationMarkerError),
    }
    if launch(
        &request.intended_cargo_bin,
        &request.session_id,
        &request.recovery_cwd,
        None,
        lease,
    ) {
        Status::Launched
    } else {
        recover(Status::LaunchFailed)
    }
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OperationMarker {
    Existing,
    Created,
    Error,
}

#[cfg(any(windows, test))]
fn consume_operation(staging_root: &Path, request: &Request) -> OperationMarker {
    use std::io::Write;

    let marker = staging_root.join(format!(
        ".operation-{}.consumed",
        sha256_hex(request.operation_id.as_bytes())
    ));
    let mut marker = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
    {
        Ok(marker) => marker,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return OperationMarker::Existing;
        }
        Err(_) => return OperationMarker::Error,
    };
    if marker.write_all(request.checksum.as_bytes()).is_ok() && marker.sync_all().is_ok() {
        OperationMarker::Created
    } else {
        OperationMarker::Error
    }
}

fn default_cargo_bin() -> anyhow::Result<PathBuf> {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".cargo")))
        .ok_or_else(|| anyhow::anyhow!("CARGO_HOME and USERPROFILE are unavailable"))?;
    anyhow::ensure!(cargo_home.is_absolute(), "CARGO_HOME is not absolute");
    canonical_absolute(&cargo_home.join("bin").join("hrdr.exe"))
        .map_err(|_| anyhow::anyhow!("default Cargo hrdr.exe is not a canonical file"))
}

#[cfg(windows)]
fn cargo_program() -> PathBuf {
    #[cfg(test)]
    if let Some(program) = std::env::var_os("HRDR_TEST_CARGO") {
        return PathBuf::from(program);
    }
    PathBuf::from("cargo")
}

#[cfg(windows)]
fn install_admitted_checkout(
    checkout_root: &Path,
    request: &Request,
    deadline: std::time::Instant,
) -> bool {
    clean_head(checkout_root, &request.expected_commit, deadline)
        && run_command(
            sanitized_command(cargo_program())
                .current_dir(checkout_root)
                .args([
                    "install",
                    "--path",
                    "apps/hrdr",
                    "--locked",
                    "--force",
                    "--bin",
                    "hrdr",
                ]),
            deadline,
            false,
        )
        .is_ok()
        && clean_head(checkout_root, &request.expected_commit, deadline)
        && default_cargo_bin().is_ok_and(|installed| installed == request.intended_cargo_bin)
}

#[cfg(all(windows, test))]
fn promote_prebuilt_candidate(candidate: &Path, destination: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, REPLACEFILE_WRITE_THROUGH,
        ReplaceFileW,
    };

    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<u16>>()
    };
    let destination_exists = destination.exists();
    let candidate = wide(candidate);
    let destination = wide(destination);
    unsafe {
        if destination_exists {
            ReplaceFileW(
                destination.as_ptr(),
                candidate.as_ptr(),
                std::ptr::null(),
                REPLACEFILE_WRITE_THROUGH,
                std::ptr::null(),
                std::ptr::null(),
            ) != 0
        } else {
            MoveFileExW(
                candidate.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            ) != 0
        }
    }
}

#[cfg(windows)]
fn sanitized_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let program = program.as_ref();
    let program = program
        .to_str()
        .filter(|program| Path::new(program).components().count() == 1)
        .map(|program| hrdr_tools::resolve_program(program, None))
        .unwrap_or_else(|| program.to_os_string());
    let mut command = Command::new(program);
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy();
        if is_git_injection_variable(&key) {
            command.env_remove(key.as_ref());
        }
    }
    command
}

#[cfg(windows)]
fn is_git_injection_variable(key: &str) -> bool {
    let matches = |candidate: &str| key.eq_ignore_ascii_case(candidate);
    [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CEILING_DIRECTORIES",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_COUNT",
    ]
    .into_iter()
    .any(matches)
        || matches("GIT_CONFIG_KEY_")
        || matches("GIT_CONFIG_VALUE_")
        || key.to_ascii_uppercase().starts_with("GIT_CONFIG_KEY_")
        || key.to_ascii_uppercase().starts_with("GIT_CONFIG_VALUE_")
}

#[cfg(windows)]
fn clean_head(checkout_root: &Path, expected_commit: &str, deadline: std::time::Instant) -> bool {
    let Ok(head) = run_command(
        sanitized_command("git")
            .current_dir(checkout_root)
            .args(["rev-parse", "--verify", "HEAD"]),
        deadline,
        true,
    ) else {
        return false;
    };
    let Ok(status) = run_command(
        sanitized_command("git").current_dir(checkout_root).args([
            "status",
            "--porcelain=v1",
            "--untracked-files=normal",
        ]),
        deadline,
        true,
    ) else {
        return false;
    };
    std::str::from_utf8(&head)
        .ok()
        .map(str::trim)
        .is_some_and(|head| head == expected_commit)
        && status.is_empty()
}

#[cfg(all(windows, test))]
fn stage_promoted_binary(
    promoted: &Path,
    staging_root: &Path,
    request: &Request,
) -> Option<(PathBuf, fs::File)> {
    use std::os::windows::fs::OpenOptionsExt;

    let extension = promoted.extension()?.to_str()?;
    let staged = staging_root.join(format!(".launch-{}.{extension}", request.checksum));
    let bytes = fs::read(promoted).ok()?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0x0000_0001 | 0x0000_0002)
        .open(&staged)
        .ok()?;
    use std::io::Write;
    output.write_all(&bytes).ok()?;
    output.sync_all().ok()?;
    drop(output);
    let guard = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&staged)
        .ok()?;
    Some((staged, guard))
}

#[cfg(windows)]
fn prebuilt_identity(
    binary: &Path,
    expected_version: &str,
    expected_commit: &str,
    expected_target: &str,
    deadline: std::time::Instant,
) -> bool {
    let Ok(output) = run_command(Command::new(binary).arg("--build-info"), deadline, true) else {
        return false;
    };
    let Ok(output) = std::str::from_utf8(&output) else {
        return false;
    };
    let expected = format!(
        "version: {expected_version}\ncommit: {expected_commit}\ntarget: {expected_target}\nsource: clean\n",
    );
    output.replace("\r\n", "\n") == expected
}

#[cfg(windows)]
fn launch(
    binary: &Path,
    session_id: &str,
    recovery_cwd: &Path,
    update_failure: Option<&str>,
    source: hrdr_tools::ProcessLease,
) -> bool {
    let mut command = Command::new(binary);
    command.current_dir(recovery_cwd);
    command.args(["__recover-handoff", session_id]);
    command.args([
        "--source-lease",
        &source.pid.to_string(),
        &source.created.to_string(),
    ]);
    if let Some(update_failure) = update_failure {
        command.args(["--update-failure", update_failure]);
    }
    command.spawn().is_ok()
}

/// Runs an argv command without a shell. Output is capped and every wait shares
/// the request deadline; a timed-out child is killed before this returns.
#[cfg(windows)]
fn run_command(
    command: &mut Command,
    deadline: std::time::Instant,
    capture: bool,
) -> Result<Vec<u8>, ()> {
    use std::io::Read;
    use std::process::Stdio;

    command.stderr(Stdio::null());
    if capture {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::null());
    }
    let mut child = command.spawn().map_err(|_| ())?;
    let reader = child.stdout.take().map(|mut stdout| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let count = stdout.read(&mut chunk).unwrap_or(0);
                if count == 0 {
                    break;
                }
                if bytes.len().saturating_add(count) > 4096 {
                    return None;
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Some(bytes)
        })
    });
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|_| ())? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            if let Some(reader) = reader {
                let _ = reader.join();
            }
            return Err(());
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    };
    if !status.success() {
        return Err(());
    }
    match reader {
        Some(reader) => reader.join().map_err(|_| ())?.ok_or(()),
        None => Ok(Vec::new()),
    }
}

fn claim(path: &Path) -> Result<PathBuf, ()> {
    if !path.is_absolute()
        || contains_control_path(path)
        || path.extension().and_then(|extension| extension.to_str()) != Some("json")
    {
        return Err(());
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| ())?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(());
    }
    let parent = path.parent().ok_or(())?;
    let canonical_parent = fs::canonicalize(parent).map_err(|_| ())?;
    let canonical_path = fs::canonicalize(path).map_err(|_| ())?;
    if canonical_path.parent() != Some(canonical_parent.as_path()) {
        return Err(());
    }
    let in_progress = canonical_path.with_extension("in-progress");
    fs::rename(canonical_path, &in_progress).map_err(|_| ())?;
    Ok(in_progress)
}

fn validate_request_path(request_path: &Path, request: &Request) -> Result<PathBuf, ()> {
    if !valid_text(&request.session_id, 128)
        || !valid_text(&request.operation_id, 128)
        || !valid_text(&request.expected_version, 64)
        || !valid_text(&request.expected_target, 256)
        || request.source_pid == 0
        || request.source_created == 0
        || request.retry_limit > MAX_RETRIES
        || request.deadline_secs == 0
        || request.deadline_secs > MAX_DEADLINE_SECS
        || !is_full_commit(&request.expected_commit)
        || !is_checksum(&request.checksum)
        || request.expected_target != env!("HRDR_BUILD_TARGET")
    {
        return Err(());
    }

    let checkout_root = canonical_absolute(&request.checkout_root)?;
    if !checkout_root.is_dir() {
        return Err(());
    }
    let recovery_cwd = canonical_absolute(&request.recovery_cwd)?;
    if !recovery_cwd.is_dir() || !recovery_cwd.starts_with(&checkout_root) {
        return Err(());
    }
    let intended_cargo_bin = canonical_absolute(&request.intended_cargo_bin)?;
    if !intended_cargo_bin.is_file()
        || default_cargo_bin().ok().as_ref() != Some(&intended_cargo_bin)
    {
        return Err(());
    }
    let operation_root = canonical_absolute(request_path.parent().ok_or(())?)?;
    if operation_root.parent() != intended_cargo_bin.parent() {
        return Err(());
    }
    let operation_hash = sha256_hex(request.operation_id.as_bytes());
    let extension = intended_cargo_bin
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("exe");
    let expected_candidate =
        operation_root.join(format!("coordinator-{operation_hash}.{extension}"));
    let candidate = canonical_absolute(&request.candidate_binary)?;
    if !candidate.is_file() || candidate != expected_candidate {
        return Err(());
    }
    let request_name = format!("request-{operation_hash}");
    let expected_request = operation_root.join(format!("{request_name}.json"));
    let expected_claimed = operation_root.join(format!("{request_name}.in-progress"));
    if request_path != expected_request && request_path != expected_claimed {
        return Err(());
    }
    Ok(checkout_root)
}

fn canonical_absolute(path: &Path) -> Result<PathBuf, ()> {
    if !path.is_absolute() || contains_control_path(path) {
        return Err(());
    }
    fs::canonicalize(path).map_err(|_| ())
}

fn contains_control_path(path: &Path) -> bool {
    path.as_os_str()
        .to_string_lossy()
        .chars()
        .any(char::is_control)
}

fn valid_text(value: &str, max_len: usize) -> bool {
    !value.is_empty() && value.len() <= max_len && !value.chars().any(char::is_control)
}

fn is_full_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_checksum(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse_request(bytes: &[u8]) -> Result<Request, ()> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| ())?;
    let object = value.as_object().ok_or(())?;
    let expected: BTreeSet<_> = [
        "schema_version",
        "session_id",
        "checkout_root",
        "recovery_cwd",
        "expected_commit",
        "expected_version",
        "candidate_binary",
        "intended_cargo_bin",
        "expected_target",
        "operation_id",
        "source_pid",
        "source_created",
        "retry_limit",
        "deadline_secs",
        "checksum",
    ]
    .into_iter()
    .collect();
    let keys: BTreeSet<_> = object.keys().map(String::as_str).collect();
    if keys != expected
        || object
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            != Some(SCHEMA_VERSION.into())
    {
        return Err(());
    }

    let string = |key| {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or(())
    };
    let number = |key| {
        object
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .ok_or(())
    };
    let request = Request {
        session_id: string("session_id")?,
        checkout_root: PathBuf::from(string("checkout_root")?),
        recovery_cwd: PathBuf::from(string("recovery_cwd")?),
        expected_commit: string("expected_commit")?,
        expected_version: string("expected_version")?,
        candidate_binary: PathBuf::from(string("candidate_binary")?),
        #[cfg(all(windows, test))]
        prebuilt_binary: PathBuf::new(),
        #[cfg(all(windows, test))]
        prebuilt_checksum: String::new(),
        intended_cargo_bin: PathBuf::from(string("intended_cargo_bin")?),
        expected_target: string("expected_target")?,
        operation_id: string("operation_id")?,
        source_pid: u32::try_from(number("source_pid")?).map_err(|_| ())?,
        source_created: number("source_created")?,
        retry_limit: u8::try_from(number("retry_limit")?).map_err(|_| ())?,
        deadline_secs: number("deadline_secs")?,
        checksum: string("checksum")?,
    };
    let checksum = sha256_hex(canonical_body(&request).as_bytes());
    (checksum == request.checksum && serialized_request(&request) == bytes)
        .then_some(request)
        .ok_or(())
}

fn canonical_body(request: &Request) -> String {
    let quote = |value: &str| serde_json::to_string(value).expect("strings serialize");
    format!(
        "{{\"schema_version\":{SCHEMA_VERSION},\"session_id\":{},\"checkout_root\":{},\"recovery_cwd\":{},\"expected_commit\":{},\"expected_version\":{},\"candidate_binary\":{},\"intended_cargo_bin\":{},\"expected_target\":{},\"operation_id\":{},\"source_pid\":{},\"source_created\":{},\"retry_limit\":{},\"deadline_secs\":{}}}",
        quote(&request.session_id),
        quote(&request.checkout_root.to_string_lossy()),
        quote(&request.recovery_cwd.to_string_lossy()),
        quote(&request.expected_commit),
        quote(&request.expected_version),
        quote(&request.candidate_binary.to_string_lossy()),
        quote(&request.intended_cargo_bin.to_string_lossy()),
        quote(&request.expected_target),
        quote(&request.operation_id),
        request.source_pid,
        request.source_created,
        request.retry_limit,
        request.deadline_secs,
    )
}

fn serialized_request(request: &Request) -> Vec<u8> {
    let body = canonical_body(request);
    format!(
        "{},\"checksum\":{}}}",
        body.trim_end_matches('}'),
        serde_json::to_string(&request.checksum).expect("checksum serializes"),
    )
    .into_bytes()
}

fn sha256_hex(input: &[u8]) -> String {
    Sha256::digest(input)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an executable launched from CARGO_HOME/bin"]
    fn prepared_request_is_accepted_by_the_strict_parser() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let (root, commit, cargo, cargo_args) = admitted_checkout(&temp);
        let previous = set_test_cargo(&cargo);
        let operation =
            hrdr_agent::UpdateOperationId::new(format!("prepared-request-{}", std::process::id()))
                .unwrap();
        let checkout = AdmittedCheckout::for_test(canonical_absolute(&root).unwrap(), commit);
        // A junction spells the checkout differently from its canonical path, as
        // an 8.3 short name does on CI runners. The session was saved under that
        // spelling, so the request must carry it unchanged.
        let alias = temp.path().join("checkout-alias");
        let junction = Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&alias)
            .arg(&root)
            .output()
            .unwrap();
        assert!(junction.status.success(), "mklink /J failed: {junction:?}");
        assert_eq!(canonical_absolute(&alias), canonical_absolute(&root));
        assert_ne!(
            canonical_absolute(&alias).unwrap(),
            alias,
            "the alias must not already be the canonical spelling"
        );
        let prepared = PreparedWindowsUpdateCoordinator::prepare(
            "saved-session",
            &operation,
            &checkout,
            &alias,
        )
        .unwrap();

        restore_test_cargo(previous);
        let request = parse_request(&fs::read(&prepared.request_path).unwrap()).unwrap();
        let lease = hrdr_tools::current_process_lease().unwrap();
        assert_eq!(request.session_id, "saved-session");
        assert_eq!(request.recovery_cwd, alias);
        assert_eq!(
            validate_request_path(&prepared.request_path, &request),
            Ok(checkout.canonical_root().to_path_buf())
        );
        assert_eq!(request.operation_id, operation.as_str());
        assert_eq!(request.source_pid, lease.pid);
        assert_eq!(request.source_created, lease.created);
        assert_eq!(request.candidate_binary, prepared.candidate_binary);
        assert_eq!(fs::read_to_string(cargo_args).unwrap(), "");
        assert_eq!(
            request.intended_cargo_bin,
            canonical_absolute(&std::env::current_exe().unwrap()).unwrap()
        );
        fs::remove_dir_all(prepared.candidate_binary.parent().unwrap()).unwrap();
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an executable launched from CARGO_HOME/bin"]
    fn prepared_coordinator_reports_spawn_failure() {
        use hrdr_tui::PreparedUpdateCoordinator;

        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let (root, commit, cargo, _) = admitted_checkout(&temp);
        let previous = set_test_cargo(&cargo);
        let checkout = AdmittedCheckout::for_test(canonical_absolute(&root).unwrap(), commit);
        let prepared = PreparedWindowsUpdateCoordinator::prepare(
            "saved-session",
            &hrdr_agent::UpdateOperationId::new(format!("spawn-failure-{}", std::process::id()))
                .unwrap(),
            &checkout,
            &root,
        )
        .unwrap();
        restore_test_cargo(previous);
        let operation_root = prepared.candidate_binary.parent().unwrap().to_path_buf();
        fs::remove_file(&prepared.candidate_binary).unwrap();

        assert!(Box::new(prepared).launch().is_err());
        fs::remove_dir_all(operation_root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn promotion_replaces_existing_destination_with_write_through() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("hrdr.exe");
        let candidate = temp.path().join("candidate.exe");
        fs::write(&destination, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();

        assert!(promote_prebuilt_candidate(&candidate, &destination));
        assert_eq!(fs::read(destination).unwrap(), b"new");
        assert!(!candidate.exists());
    }

    #[cfg(windows)]
    #[test]
    fn promotion_creates_absent_destination_with_write_through() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("hrdr.exe");
        let candidate = temp.path().join("candidate.exe");
        fs::write(&candidate, b"new").unwrap();

        assert!(promote_prebuilt_candidate(&candidate, &destination));
        assert_eq!(fs::read(destination).unwrap(), b"new");
        assert!(!candidate.exists());
    }

    #[cfg(windows)]
    #[test]
    fn promotion_sharing_violation_preserves_destination_and_candidate() {
        use std::os::windows::fs::OpenOptionsExt;

        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("hrdr.exe");
        let candidate = temp.path().join("candidate.exe");
        fs::write(&destination, b"old").unwrap();
        fs::write(&candidate, b"new").unwrap();
        let guard = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&destination)
            .unwrap();

        assert!(!promote_prebuilt_candidate(&candidate, &destination));
        assert_eq!(fs::read(destination).unwrap(), b"old");
        assert_eq!(fs::read(candidate).unwrap(), b"new");
        drop(guard);
    }

    #[cfg(windows)]
    #[test]
    fn prebuilt_identity_rejects_mismatched_build_info() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("prebuilt.cmd");
        fs::write(
            &binary,
            "@echo off\r\necho version: mismatched\r\necho commit: deadbeef\r\necho target: mismatched\r\necho source: dirty\r\n",
        )
        .unwrap();

        assert!(!prebuilt_identity(
            &binary,
            env!("CARGO_PKG_VERSION"),
            &"0".repeat(40),
            env!("HRDR_BUILD_TARGET"),
            std::time::Instant::now() + std::time::Duration::from_secs(2),
        ));
    }

    #[cfg(windows)]
    #[test]
    fn prebuilt_build_rejects_dirty_admitted_checkout() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let (root, commit, _, _) = admitted_checkout(&temp);
        fs::write(root.join("untracked"), b"dirty").unwrap();

        assert!(!clean_head(
            &root,
            &commit,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
        ));
    }

    #[cfg(windows)]
    #[test]
    fn prebuilt_checksum_tampering_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let operation_hash = sha256_hex(b"operation");
        let root = temp.path();
        let intended = root.join("bin/hrdr.exe");
        fs::create_dir_all(intended.parent().unwrap()).unwrap();
        fs::write(&intended, b"installed").unwrap();
        let operation_root = intended
            .parent()
            .unwrap()
            .join(format!(".hrdr-update-{operation_hash}"));
        fs::create_dir(&operation_root).unwrap();
        let candidate = operation_root.join(format!("coordinator-{operation_hash}.exe"));
        fs::write(&candidate, b"coordinator").unwrap();
        let prebuilt = operation_root
            .join("target")
            .join(env!("HRDR_BUILD_TARGET"))
            .join("release")
            .join(if cfg!(test) { "hrdr.cmd" } else { "hrdr.exe" });
        fs::create_dir_all(prebuilt.parent().unwrap()).unwrap();
        fs::write(&prebuilt, b"prebuilt").unwrap();
        let request_path = operation_root.join(format!("request-{operation_hash}.json"));
        let mut signed = request(root, candidate);
        signed.intended_cargo_bin = intended.clone();
        signed.prebuilt_binary = prebuilt.clone();
        signed.prebuilt_checksum = sha256_hex(b"prebuilt");
        write_request(&request_path, &mut signed);
        fs::write(&prebuilt, b"tampered").unwrap();

        assert!(parse_request(&fs::read(&request_path).unwrap()).is_ok());
        assert!(validate_request_path(&request_path, &signed).is_err());
        assert_eq!(fs::read(&intended).unwrap(), b"installed");
        assert_eq!(fs::read(&prebuilt).unwrap(), b"tampered");
    }

    #[test]
    fn sha256_matches_known_value() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn write_request(path: &Path, request: &mut Request) {
        request.checksum = sha256_hex(canonical_body(request).as_bytes());
        let body = canonical_body(request).trim_end_matches('}').to_owned();
        fs::write(
            path,
            format!("{body},\"checksum\":\"{}\"}}", request.checksum),
        )
        .unwrap();
    }

    #[cfg(windows)]
    fn admitted_checkout(temp: &tempfile::TempDir) -> (PathBuf, String, PathBuf, PathBuf) {
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join("tracked"), b"tracked").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "tracked"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "test",
                ])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        let commit = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        let cargo_args = temp.path().join("cargo-args.txt");
        let cargo = temp.path().join("cargo.cmd");
        fs::write(
            &cargo,
            format!(
                "@echo off\r\nif \"%1\"==\"--build-info\" (\r\necho version: {}\r\necho commit: {commit}\r\necho target: {}\r\necho source: clean\r\nexit /b 0\r\n)\r\necho %*>>\"{}\"\r\nif not \"%1\"==\"build\" exit /b 1\r\nmkdir \"%9\\{}\\release\"\r\ncopy \"%~f0\" \"%9\\{}\\release\\hrdr.cmd\" >nul\r\n",
                env!("CARGO_PKG_VERSION"),
                env!("HRDR_BUILD_TARGET"),
                cargo_args.display(),
                env!("HRDR_BUILD_TARGET"),
                env!("HRDR_BUILD_TARGET"),
            ),
        )
        .unwrap();
        (root, commit, cargo, cargo_args)
    }

    #[cfg(windows)]
    fn set_test_cargo(cargo: &Path) -> Option<std::ffi::OsString> {
        let previous = std::env::var_os("HRDR_TEST_CARGO");
        unsafe { std::env::set_var("HRDR_TEST_CARGO", cargo) };
        previous
    }

    #[cfg(windows)]
    fn restore_test_cargo(previous: Option<std::ffi::OsString>) {
        unsafe {
            match previous {
                Some(value) => std::env::set_var("HRDR_TEST_CARGO", value),
                None => std::env::remove_var("HRDR_TEST_CARGO"),
            }
        }
    }

    fn request(root: &Path, candidate: PathBuf) -> Request {
        Request {
            session_id: "session".into(),
            checkout_root: root.to_path_buf(),
            recovery_cwd: root.to_path_buf(),
            expected_commit: "0".repeat(40),
            expected_version: env!("CARGO_PKG_VERSION").into(),
            candidate_binary: candidate,
            #[cfg(windows)]
            prebuilt_binary: root.join("prebuilt"),
            #[cfg(windows)]
            prebuilt_checksum: sha256_hex(b""),
            intended_cargo_bin: root.join("bin/hrdr"),
            expected_target: env!("HRDR_BUILD_TARGET").into(),
            operation_id: "operation".into(),
            source_pid: 1,
            source_created: 1,
            retry_limit: 1,
            deadline_secs: 60,
            checksum: String::new(),
        }
    }

    #[test]
    fn malformed_tampered_and_reused_requests_are_not_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let malformed = temp.path().join("malformed.json");
        fs::write(&malformed, b"not json").unwrap();
        assert_eq!(run(malformed.clone()), Status::Invalid);
        assert_eq!(run(malformed), Status::Reused);

        let tampered = temp.path().join("tampered.json");
        let mut signed = request(temp.path(), temp.path().join("candidate"));
        write_request(&tampered, &mut signed);
        let mut tampered_bytes = serialized_request(&signed);
        tampered_bytes.insert(1, b' ');
        assert!(parse_request(&tampered_bytes).is_err());
        let tampered_cwd = temp.path().join("outside");
        fs::create_dir(&tampered_cwd).unwrap();
        let recovery_cwd =
            serde_json::to_string(&signed.recovery_cwd.display().to_string()).unwrap();
        let replacement = serde_json::to_string(&tampered_cwd.display().to_string()).unwrap();
        let body = fs::read_to_string(&tampered).unwrap().replace(
            &format!("\"recovery_cwd\":{recovery_cwd}"),
            &format!("\"recovery_cwd\":{replacement}"),
        );
        fs::write(&tampered, body).unwrap();
        assert_eq!(run(tampered), Status::Invalid);
    }

    #[test]
    fn missing_process_instance_fields_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("request.json");
        let mut signed = request(temp.path(), temp.path().join("candidate"));
        signed.source_created = 0;
        write_request(&path, &mut signed);
        assert_eq!(run(path), Status::Invalid);
    }

    #[cfg(windows)]
    #[test]
    fn launch_passes_one_exact_handoff_argv() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("launches.txt");
        let script = temp.path().join("installed.cmd");
        fs::write(
            &script,
            format!(
                "@echo off\r\necho %cd%^|%1^|%2^|%3^|%4^|%5>>\"{}\"\r\n",
                marker.display()
            ),
        )
        .unwrap();

        let source = hrdr_tools::ProcessLease {
            pid: 123,
            created: 456,
        };
        assert!(launch(&script, "saved-session", temp.path(), None, source));
        let expected = format!(
            "{}|__recover-handoff|saved-session|--source-lease|123|456\r\n",
            temp.path().display()
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while fs::read_to_string(&marker).ok().as_deref() != Some(expected.as_str())
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(fs::read_to_string(marker).unwrap(), expected);
    }

    #[cfg(windows)]
    #[test]
    fn staged_binary_guard_denies_concurrent_write_and_delete() {
        let temp = tempfile::tempdir().unwrap();
        let installed = temp.path().join("installed.exe");
        let staging = temp.path().join("staging");
        fs::create_dir(&staging).unwrap();
        fs::write(&installed, b"installed binary").unwrap();
        let request = request(temp.path(), staging.join("candidate.exe"));
        let (staged, guard) = stage_promoted_binary(&installed, &staging, &request).unwrap();

        assert!(fs::OpenOptions::new().write(true).open(&staged).is_err());
        assert!(fs::remove_file(&staged).is_err());

        drop(guard);
        fs::remove_file(staged).unwrap();
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "superseded by Cargo-install coordinator E2E"]
    fn git_config_parameters_worktree_redirect_is_removed_and_promotion_never_runs_cargo() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(
            root.join(".gitignore"),
            "fake-bin/\ncargo-home/\ncargo-args.txt\nlaunches.txt\nredirected-worktree/\n",
        )
        .unwrap();
        assert!(
            Command::new("git")
                .args(["add", ".gitignore"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "test"
                ])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        let commit = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        let bin = root.join("fake-bin");
        fs::create_dir_all(&bin).unwrap();
        let cargo_args = root.join("cargo-args.txt");
        fs::write(
            bin.join("cargo.cmd"),
            format!(
                "@echo off\r\nif defined GIT_DIR exit /b 2\r\nif defined GIT_CONFIG_PARAMETERS exit /b 3\r\nif \"%1\"==\"metadata\" (\r\necho {{\"packages\":[{{\"name\":\"hrdr\",\"version\":\"{}\"}}]}}\r\nexit /b 0\r\n)\r\necho %*>>\"{}\"\r\nexit /b 0\r\n",
                env!("CARGO_PKG_VERSION"),
                cargo_args.display()
            ),
        )
        .unwrap();
        let cargo_home = root.join("cargo-home");
        let installed = cargo_home.join("bin/hrdr.cmd");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        let marker = root.join("launches.txt");
        fs::write(
            &installed,
            format!(
                "@echo off\r\nif \"%1\"==\"--build-info\" (\r\necho version: {}\r\necho commit: {commit}\r\necho target: {}\r\necho source: clean\r\nexit /b 0\r\n)\r\necho %1^|%2>>\"{}\"\r\n",
                env!("CARGO_PKG_VERSION"),
                env!("HRDR_BUILD_TARGET"),
                marker.display(),
            ),
        ).unwrap();
        let operation_hash = sha256_hex(b"operation");
        let staging = installed
            .parent()
            .unwrap()
            .join(format!(".hrdr-update-{operation_hash}"));
        fs::create_dir(&staging).unwrap();
        let candidate = staging.join(format!("coordinator-{operation_hash}.cmd"));
        fs::write(&candidate, b"candidate").unwrap();
        let prebuilt = staging
            .join("target")
            .join(env!("HRDR_BUILD_TARGET"))
            .join("release")
            .join(if cfg!(test) { "hrdr.cmd" } else { "hrdr.exe" });
        fs::create_dir_all(prebuilt.parent().unwrap()).unwrap();
        fs::write(
            &prebuilt,
            format!(
                "@echo off\r\nif \"%1\"==\"--build-info\" (\r\necho version: {}\r\necho commit: {commit}\r\necho target: {}\r\necho source: clean\r\nexit /b 0\r\n)\r\necho %1^|%2>>\"{}\"\r\n",
                env!("CARGO_PKG_VERSION"),
                env!("HRDR_BUILD_TARGET"),
                marker.display(),
            ),
        )
        .unwrap();
        let previous_test_cargo = std::env::var_os("HRDR_TEST_CARGO");
        let previous_cargo_home = std::env::var_os("CARGO_HOME");
        let previous_git_dir = std::env::var_os("GIT_DIR");
        let previous_git_config_parameters = std::env::var_os("GIT_CONFIG_PARAMETERS");
        let redirected_worktree = root.join("redirected-worktree");
        fs::create_dir_all(&redirected_worktree).unwrap();
        fs::write(redirected_worktree.join("tampered"), b"tampered").unwrap();
        unsafe {
            std::env::set_var("HRDR_TEST_CARGO", bin.join("cargo.cmd"));
            std::env::set_var("CARGO_HOME", &cargo_home);
            std::env::set_var("GIT_DIR", root.join("redirected-git-dir"));
            std::env::set_var(
                "GIT_CONFIG_PARAMETERS",
                format!(
                    "'core.worktree={}'",
                    redirected_worktree.to_string_lossy().replace('\\', "/")
                ),
            );
        }
        let configured_worktree = Command::new("git")
            .env_remove("GIT_DIR")
            .args(["config", "--get", "core.worktree"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(configured_worktree.status.success());
        assert_eq!(
            String::from_utf8(configured_worktree.stdout)
                .unwrap()
                .trim(),
            redirected_worktree.to_string_lossy().replace('\\', "/"),
        );
        let request_path = staging.join(format!("request-{operation_hash}.json"));
        let mut signed = request(root, candidate);
        signed.expected_commit = commit;
        signed.intended_cargo_bin = installed;
        signed.prebuilt_binary = prebuilt;
        signed.prebuilt_checksum = sha256_hex(&fs::read(&signed.prebuilt_binary).unwrap());
        signed.source_pid = u32::MAX;
        signed.source_created = 1;
        write_request(&request_path, &mut signed);
        assert!(clean_head(
            root,
            &signed.expected_commit,
            std::time::Instant::now() + std::time::Duration::from_secs(2)
        ));
        let result = run(request_path);
        unsafe {
            match previous_test_cargo {
                Some(value) => std::env::set_var("HRDR_TEST_CARGO", value),
                None => std::env::remove_var("HRDR_TEST_CARGO"),
            }
            match previous_cargo_home {
                Some(value) => std::env::set_var("CARGO_HOME", value),
                None => std::env::remove_var("CARGO_HOME"),
            }
            match previous_git_dir {
                Some(value) => std::env::set_var("GIT_DIR", value),
                None => std::env::remove_var("GIT_DIR"),
            }
            match previous_git_config_parameters {
                Some(value) => std::env::set_var("GIT_CONFIG_PARAMETERS", value),
                None => std::env::remove_var("GIT_CONFIG_PARAMETERS"),
            }
        }
        assert_eq!(result, Status::Launched);
        assert!(
            Command::new("git")
                .args(["status", "--porcelain=v1", "--untracked-files=normal"])
                .current_dir(root)
                .output()
                .unwrap()
                .stdout
                .is_empty()
        );
        assert!(!cargo_args.exists());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            fs::read_to_string(marker).unwrap(),
            "__recover-handoff|session\r\n"
        );
    }
    #[test]
    fn operation_marker_consumes_byte_identical_requests_once() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("candidate");
        fs::write(&candidate, b"candidate").unwrap();
        let mut first = request(temp.path(), candidate.clone());
        first.checksum = sha256_hex(canonical_body(&first).as_bytes());
        let mut second = request(temp.path(), candidate);
        second.checksum = first.checksum.clone();
        assert_eq!(
            consume_operation(temp.path(), &first),
            OperationMarker::Created
        );
        assert_eq!(
            consume_operation(temp.path(), &second),
            OperationMarker::Existing
        );
    }

    #[test]
    fn operation_marker_reports_io_errors() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("candidate");
        fs::write(&candidate, b"candidate").unwrap();
        let mut request = request(temp.path(), candidate.clone());
        request.checksum = sha256_hex(canonical_body(&request).as_bytes());
        assert_eq!(
            consume_operation(&candidate, &request),
            OperationMarker::Error
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an executable launched from CARGO_HOME/bin"]
    fn coordinator_staging_does_not_dirty_the_admitted_checkout() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let (checkout_root, commit, cargo, _) = admitted_checkout(&temp);
        let previous = set_test_cargo(&cargo);
        let checkout =
            AdmittedCheckout::for_test(canonical_absolute(&checkout_root).unwrap(), commit);
        let prepared = PreparedWindowsUpdateCoordinator::prepare(
            "saved-session",
            &hrdr_agent::UpdateOperationId::new(format!(
                "clean-checkout-staging-{}",
                std::process::id()
            ))
            .unwrap(),
            &checkout,
            &checkout_root,
        )
        .unwrap();
        restore_test_cargo(previous);
        let operation_root = prepared.candidate_binary.parent().unwrap().to_path_buf();

        assert!(
            Command::new("git")
                .args(["status", "--porcelain=v1", "--untracked-files=all"])
                .current_dir(&checkout_root)
                .output()
                .unwrap()
                .stdout
                .is_empty()
        );
        assert!(!checkout_root.join(".hrdr").exists());
        assert_eq!(
            operation_root.parent(),
            canonical_absolute(&std::env::current_exe().unwrap())
                .unwrap()
                .parent()
        );
        let request = parse_request(&fs::read(&prepared.request_path).unwrap()).unwrap();
        assert_eq!(
            validate_request_path(&prepared.request_path, &request),
            Ok(checkout.canonical_root().to_path_buf())
        );

        fs::remove_dir_all(operation_root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an executable launched from CARGO_HOME/bin"]
    fn offroot_candidate_is_rejected_before_git() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let (root, commit, cargo, _) = admitted_checkout(&temp);
        let previous = set_test_cargo(&cargo);
        let checkout = AdmittedCheckout::for_test(canonical_absolute(&root).unwrap(), commit);
        let prepared = PreparedWindowsUpdateCoordinator::prepare(
            "saved-session",
            &hrdr_agent::UpdateOperationId::new(format!(
                "offroot-candidate-{}",
                std::process::id()
            ))
            .unwrap(),
            &checkout,
            &root,
        )
        .unwrap();
        restore_test_cargo(previous);
        let operation_root = prepared.candidate_binary.parent().unwrap().to_path_buf();
        let outside = temp.path().join("candidate.exe");
        fs::write(&outside, b"candidate").unwrap();
        let mut signed = parse_request(&fs::read(&prepared.request_path).unwrap()).unwrap();
        signed.candidate_binary = outside;
        write_request(&prepared.request_path, &mut signed);

        assert_eq!(run(prepared.request_path), Status::Invalid);
        fs::remove_dir_all(operation_root).unwrap();
    }
}

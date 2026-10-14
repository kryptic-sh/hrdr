//! Self-update on unix: install while the session keeps running, then replace
//! this process's image with the installed binary.
//!
//! Windows needs a separate coordinator because a running executable cannot be
//! replaced there. Unix has neither problem: `cargo install` may overwrite the
//! binary this process runs from, and `exec` keeps the PID, the start time and
//! — what a separately launched process could not have — the foreground of the
//! terminal. So the install and its attestation run before the handoff commits,
//! and a failure there leaves the session untouched; after the commit the TUI
//! restores the terminal and `main` execs the successor, which claims the
//! checkpoint as itself (`hrdr_tui::RecoveryClaim::Successor`).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::update_check::AdmittedCheckout;
use crate::update_coordinator::{
    cargo_program, clean_head, default_cargo_bin, prebuilt_identity, run_command_with_stderr,
    sanitized_command,
};

/// How long the install and its checks may take in all. A cold release build
/// of hrdr takes minutes; past this the update is abandoned and the session
/// carries on.
const INSTALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// What `main` execs once the TUI has exited for a committed update handoff.
struct ExecPlan {
    binary: PathBuf,
    session_id: String,
    recovery_cwd: PathBuf,
}

/// The plan a committed handoff left for `main`; set at most once per process.
static PLAN: Mutex<Option<ExecPlan>> = Mutex::new(None);

/// An update installed and attested, waiting for its handoff to commit.
pub(crate) struct PreparedExec {
    plan: ExecPlan,
}

impl PreparedExec {
    /// Install the admitted checkout into the default Cargo location and check
    /// the installed binary is exactly that commit, before anything about the
    /// session changes. Every failure leaves the running session as it was.
    pub(crate) fn prepare(
        session_id: &str,
        checkout: &AdmittedCheckout,
        recovery_cwd: &Path,
    ) -> anyhow::Result<Self> {
        let installed = default_cargo_bin()?;
        let current = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|error| anyhow::anyhow!("locating the current executable: {error}"))?;
        anyhow::ensure!(
            current == installed,
            "the running hrdr is not the default Cargo install, so updating it would not \
             change what runs"
        );
        let checkout_root = checkout.canonical_root();
        // The successor reopens the session by the exact cwd string it was saved
        // under; the canonical form only proves it lies inside the checkout.
        let canonical_cwd = std::fs::canonicalize(recovery_cwd)
            .map_err(|error| anyhow::anyhow!("resolving the session directory: {error}"))?;
        anyhow::ensure!(
            canonical_cwd.starts_with(checkout_root),
            "the session directory is outside the admitted checkout"
        );
        let commit = checkout.expected_commit();
        let deadline = std::time::Instant::now() + INSTALL_DEADLINE;
        anyhow::ensure!(
            clean_head(checkout_root, commit, deadline),
            "the checkout is no longer clean at the admitted commit"
        );
        // Cargo's error output goes to a log, so a failure can say what it was.
        let log_path = std::env::temp_dir().join(format!(
            "hrdr-update-install-{}-{}.log",
            std::process::id(),
            hrdr_tools::unix_now()
        ));
        let log = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&log_path)
            .map_err(|error| anyhow::anyhow!("creating the install log: {error}"))?;
        let stderr = log
            .try_clone()
            .map_err(|error| anyhow::anyhow!("opening the install log: {error}"));
        let installed_ok = stderr.and_then(|stderr| {
            run_command_with_stderr(
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
                stderr.into(),
            )
            .map_err(|()| {
                anyhow::anyhow!(
                    "`cargo install` of the admitted checkout failed:\n{}",
                    log_tail(&log)
                )
            })
        });
        let _ = std::fs::remove_file(&log_path);
        installed_ok?;
        anyhow::ensure!(
            clean_head(checkout_root, commit, deadline),
            "the checkout changed while it was being installed"
        );
        anyhow::ensure!(
            prebuilt_identity(
                &installed,
                env!("CARGO_PKG_VERSION"),
                commit,
                env!("HRDR_BUILD_TARGET"),
                deadline,
            ),
            "the installed hrdr does not report the admitted commit"
        );
        Ok(Self {
            plan: ExecPlan {
                binary: installed,
                session_id: session_id.to_owned(),
                recovery_cwd: recovery_cwd.to_path_buf(),
            },
        })
    }
}

/// The end of a command's error output, enough to name what went wrong without
/// flooding the TUI with a whole build log.
fn log_tail(log: &std::fs::File) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let mut log = log;

    /// Bytes of the log's end that a failure quotes.
    const TAIL_BYTES: u64 = 2048;
    let length = log.seek(SeekFrom::End(0)).unwrap_or(0);
    let _ = log.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES)));
    let mut tail = Vec::new();
    let _ = log.read_to_end(&mut tail);
    let tail = String::from_utf8_lossy(&tail).trim().to_string();
    if tail.is_empty() {
        "(cargo printed nothing)".to_string()
    } else {
        tail
    }
}

impl hrdr_tui::PreparedUpdateCoordinator for PreparedExec {
    /// Leave the plan for `main`: the exec must wait until the TUI has given the
    /// terminal back.
    fn launch(self: Box<Self>) -> anyhow::Result<()> {
        let mut plan = PLAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(plan.is_none(), "an update restart is already pending");
        *plan = Some(self.plan);
        Ok(())
    }
}

/// Replace this process with the successor a committed update handoff left,
/// if one did. Returns only on failure, with an error that says how to recover:
/// the handoff stays committed, and the next start in the session's directory
/// resumes it.
pub(crate) fn exec_pending() -> Option<anyhow::Error> {
    use std::os::unix::process::CommandExt;

    let plan = PLAN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()?;
    let error = std::process::Command::new(&plan.binary)
        .current_dir(&plan.recovery_cwd)
        .args(["__recover-handoff", &plan.session_id, "--exec-successor"])
        .exec();
    Some(anyhow::anyhow!(
        "hrdr was updated, but restarting into {} failed: {error}. Start hrdr in {} to resume \
         the session.",
        plan.binary.display(),
        plan.recovery_cwd.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrdr_tui::PreparedUpdateCoordinator;

    /// A committed handoff leaves exactly one plan; a second launch is refused,
    /// and an exec that cannot start says how to resume rather than leaving the
    /// user with a bare OS error. (The successful exec replaces the test
    /// process, so it is covered end to end, not here.)
    #[test]
    fn a_plan_is_left_once_and_a_failed_exec_says_how_to_resume() {
        let dir = tempfile::tempdir().unwrap();
        let plan = || ExecPlan {
            binary: dir.path().join("no-such-hrdr"),
            session_id: "saved".into(),
            recovery_cwd: dir.path().to_path_buf(),
        };
        Box::new(PreparedExec { plan: plan() }).launch().unwrap();
        let again = Box::new(PreparedExec { plan: plan() })
            .launch()
            .unwrap_err();
        assert!(again.to_string().contains("already pending"), "{again}");

        let error = exec_pending().expect("the plan was taken and the exec failed");
        let message = error.to_string();
        assert!(message.contains("no-such-hrdr"), "{message}");
        assert!(message.contains("Start hrdr in"), "{message}");
        assert!(exec_pending().is_none(), "the plan is consumed");
    }
}

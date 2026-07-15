//! Kill the whole process *tree*, not just the direct child.
//!
//! Every subprocess this crate spawns is started with `kill_on_drop(true)`
//! and, on the explicit kill paths (timeout, cancel), an extra kill call.
//! Both of those act on the single pid tokio spawned — the shell leader.
//! That is not enough: `bash -c "npm run dev"` forks `node`, and killing only
//! `bash` leaves `node` running (holding its port) forever. "Esc stops
//! everything" needs to mean everything.
//!
//! The fix is platform-specific, but both cover the two paths that matter — an
//! *explicit* kill (timeout/overflow) and a *dropped* future (Esc-cancelled
//! turn, LSP registry teardown):
//!
//! * **Unix** — [`configure`] puts the child in a *new process group* of its
//!   own (`pgid == pid`) before it is spawned. [`ProcessGroup::kill`] signals
//!   the *negative* pid — the whole group — which reaches every descendant that
//!   hasn't itself broken out of the group (e.g. with its own `setsid`). The
//!   guard also carries the pgid and its `Drop` repeats that group-kill, so a
//!   future being torn down without an explicit kill (the Esc/cancel path, which
//!   `handle.abort()`s the turn and drops its locals) still takes the tree down
//!   — not just the leader that `kill_on_drop` reaps.
//! * **Windows** — there is no process-group equivalent, so each child is
//!   assigned to a Windows *Job Object* carrying
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: every process in the job (and
//!   anything it spawns, unless that child opts out with
//!   `CREATE_BREAKAWAY_FROM_JOB`) is terminated the moment the job is
//!   explicitly terminated *or* its last handle is closed — so dropping the
//!   [`ProcessGroup`] guard kills the tree too. A `taskkill /T /F` shell-out was
//!   the other option; a Job Object was chosen because it doesn't spawn yet
//!   another process to do the killing.
//!
//! Known limitation (Windows only): the child is spawned running and assigned to
//! the job a moment later, so a descendant forked in that narrow window escapes
//! the job. The race-free form (`CREATE_SUSPENDED` → assign → `ResumeThread`) is
//! awkward through tokio's spawn API; the window is tiny and this is a
//! best-effort resource guard, not a security boundary. Unix has no such race —
//! `process_group(0)` is applied pre-exec, atomically with the spawn.

use std::io;

/// Put `cmd`'s future child in a position to have its whole process tree
/// killed later, not just its own pid. Call this before `spawn()`, alongside
/// the stdio/`kill_on_drop` setup every call site already does.
///
/// Unix: makes the child the leader of a brand new process group
/// (`process_group(0)` — pgid becomes the child's own pid), so
/// [`ProcessGroup::kill`] can later signal `-pgid` for the whole group.
///
/// Windows: a no-op here. Grouping happens after spawn, once there is a
/// process handle to assign into a Job Object — see [`ProcessGroup::attach`].
pub(crate) fn configure(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        let _ = cmd;
    }
}

/// Handle to whatever OS resource lets [`kill`](ProcessGroup::kill) take down
/// an entire process tree spawned through a [`configure`]d `Command`.
///
/// Unix carries the leader pid (which *is* the group's pgid) so both an explicit
/// [`kill`](ProcessGroup::kill) and the guard's `Drop` can signal `-pgid`.
/// Windows carries the Job Object the child was assigned to.
pub(crate) struct ProcessGroup {
    #[cfg(unix)]
    pgid: Option<u32>,
    #[cfg(windows)]
    job: windows_job::Job,
}

impl ProcessGroup {
    /// Attach to `child` right after spawning it (before awaiting anything
    /// else on it). Unix: records the leader pid (the group already exists
    /// because `cmd` was [`configure`]d before `spawn()`). Windows: creates a
    /// kill-on-close Job Object and assigns `child` to it.
    pub(crate) fn attach(child: &tokio::process::Child) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self { pgid: child.id() })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                job: windows_job::Job::new_and_assign(child)?,
            })
        }
    }

    /// Kill every process in the tree, now. `pid` is the child's own pid, as
    /// reported by `Child::id()` right after spawn — used on unix to target
    /// `-pid` (the whole process group); ignored on windows, where the job
    /// handle alone identifies the whole tree.
    ///
    /// A `pid` of `None` (the child already reaped) is a silent no-op. The
    /// guard's `Drop` performs the same group-kill, so the explicit call here
    /// is really just "don't wait for the drop" on the timeout/overflow paths.
    pub(crate) fn kill(&self, pid: Option<u32>) {
        #[cfg(unix)]
        {
            if let Some(pid) = pid {
                unix_group_kill(pid);
            }
        }
        #[cfg(windows)]
        {
            let _ = pid;
            self.job.terminate();
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // The drop-path backstop: a cancelled turn `abort()`s its task, which
        // drops this guard's locals without calling `kill()`. `kill_on_drop`
        // only reaps the leader; this takes the whole group down, so "Esc stops
        // everything" holds on unix too (matching the Windows job-handle-close
        // behaviour). Harmless on the normal path — the group is already empty
        // (ESRCH), which we ignore.
        if let Some(pgid) = self.pgid {
            unix_group_kill(pgid);
        }
    }
}

/// SIGKILL the whole process group led by `pid` (`kill(-pid)`).
///
/// Guards `pid > 1`: `-0` would signal the *caller's* own group and `-1` every
/// process on the system. A real child pid from `Child::id()` is always `> 1`,
/// so this only ever hardens against future misuse. ESRCH (group already gone)
/// is the common, ignored case.
#[cfg(unix)]
fn unix_group_kill(pid: u32) {
    if pid > 1 {
        // SAFETY: `libc::kill` is a plain syscall wrapper; with any argument it
        // can only fail (ESRCH/EPERM), never cause undefined behaviour.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
mod windows_job {
    use std::io;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    /// A Windows Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` set:
    /// every process assigned to it — and anything *that* spawns, unless it
    /// opts out with `CREATE_BREAKAWAY_FROM_JOB` — is terminated the moment
    /// the job is explicitly terminated or its last handle is closed. This is
    /// the Windows analogue of a unix process group plus `kill(-pgid)`.
    pub(crate) struct Job {
        handle: windows_sys::Win32::Foundation::HANDLE,
    }

    // The HANDLE is an opaque kernel-object reference; nothing about it is
    // thread-affine, so it's fine to hold across await points / move between
    // the tokio runtime's worker threads.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// Create a fresh kill-on-close job and assign `child` to it.
        pub(crate) fn new_and_assign(child: &tokio::process::Child) -> io::Result<Self> {
            // SAFETY: FFI calls per the documented Win32 Job Object API,
            // using out-parameters/handles exactly as their signatures
            // require; every failure path is checked and propagated.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle == 0 {
                return Err(io::Error::last_os_error());
            }
            let job = Self { handle };

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = unsafe {
                SetInformationJobObject(
                    job.handle,
                    JobObjectExtendedLimitInformation,
                    std::ptr::addr_of!(info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error()); // `job`'s Drop closes the handle
            }

            let Some(raw) = child.raw_handle() else {
                return Err(io::Error::other("child has already exited"));
            };
            let ok = unsafe { AssignProcessToJobObject(job.handle, raw as isize) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        /// Terminate everything in the job immediately.
        pub(crate) fn terminate(&self) {
            // SAFETY: `handle` is a valid Job Object handle for the lifetime
            // of `self`.
            unsafe {
                TerminateJobObject(self.handle, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // Closing the job's last handle terminates anything still in it
            // (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) — the drop-path backstop,
            // mirroring what `kill_on_drop` gives the leader pid on unix, but
            // covering the whole tree.
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    /// A grandchild started by a `configure`d command is reachable through
    /// its group: killing `-pid` (not just `pid`) actually reaches it.
    ///
    /// This exercises the primitive directly (spawn a shell that backgrounds
    /// a sleep, then kill the group) rather than going through a `Tool`, to
    /// keep the unit test fast and independent of any particular call site's
    /// plumbing. The end-to-end version (via the `bash` tool's timeout path)
    /// lives in `tools/shell.rs`.
    #[tokio::test]
    async fn killing_the_group_reaches_a_backgrounded_grandchild() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("still-alive");
        let pid_file = dir.path().join("child.pid");

        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(format!(
            "(sleep 5 && touch {m}) & echo $! > {p}; wait",
            m = marker.display(),
            p = pid_file.display(),
        ));
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        super::configure(&mut cmd);

        let mut child = cmd.spawn().unwrap();
        let pid = child.id();
        let group = super::ProcessGroup::attach(&child).unwrap();

        // Give the backgrounded `sleep` a moment to actually start before we
        // kill the group out from under it.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let grandchild_pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        group.kill(pid);
        let _ = child.kill().await; // reap the leader, as every real call site does

        // The grandchild must be gone almost immediately — well before its
        // own 5s sleep would have finished on its own.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let alive = unsafe { libc::kill(grandchild_pid, 0) == 0 };
        assert!(
            !alive,
            "grandchild pid {grandchild_pid} survived a group kill"
        );
        assert!(
            !marker.exists(),
            "the grandchild's sleep completed — it was never actually killed"
        );
    }

    /// The Esc/cancel path drops the future's locals without calling `kill()`.
    /// Dropping the [`ProcessGroup`](super::ProcessGroup) guard must still take
    /// the whole tree down on unix, not just the leader `kill_on_drop` reaps.
    #[tokio::test]
    async fn dropping_the_guard_kills_the_group_not_just_the_leader() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("still-alive");
        let pid_file = dir.path().join("child.pid");

        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(format!(
            "(sleep 5 && touch {m}) & echo $! > {p}; wait",
            m = marker.display(),
            p = pid_file.display(),
        ));
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        super::configure(&mut cmd);

        let child = cmd.spawn().unwrap();
        let group = super::ProcessGroup::attach(&child).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let grandchild_pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        // Simulate the cancelled future's teardown: drop the guard (group-kill
        // via `Drop`) and the child (`kill_on_drop` reaps the leader). No
        // explicit `kill()` call — that is the whole point.
        drop(group);
        drop(child);

        tokio::time::sleep(Duration::from_millis(200)).await;
        let alive = unsafe { libc::kill(grandchild_pid, 0) == 0 };
        assert!(
            !alive,
            "grandchild pid {grandchild_pid} survived the guard being dropped"
        );
        assert!(!marker.exists());
    }
}

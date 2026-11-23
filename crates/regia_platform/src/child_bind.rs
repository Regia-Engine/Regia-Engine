// SPDX-License-Identifier: MPL-2.0
//! Child processes that end with their parent (ADR-2682): the editor's play-in-editor server and
//! client processes must not outlive the editor, however it ends.
//!
//! - Linux: [`bind_to_parent`] sets the parent-death signal (`prctl(PR_SET_PDEATHSIG, SIGKILL)`) in
//!   the child before `exec`. The kernel sends it when the *thread* that spawned the child ends, so
//!   spawn from a thread that lives as long as the session (the editor's main thread).
//! - Windows: a [`ChildGroup`] is a job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; the system
//!   closes the job's handle when the editor process ends, which ends every process in it.
//! - Elsewhere (macOS, Android): best effort; the owner kills its children on stop and on a clean
//!   exit ([`ChildGroup`] adds nothing).

use std::process::{Child, Command};

/// Asks the OS to end the process `cmd` starts when this process's spawning thread ends (Linux);
/// does nothing on other systems.
pub fn bind_to_parent(cmd: &mut Command) {
    imp::bind_to_parent(cmd);
}

/// Child processes that the OS ends when this process ends (a job object on Windows; nothing on
/// other systems, where [`bind_to_parent`] or the owner does it).
pub struct ChildGroup {
    inner: imp::Group,
}

impl std::fmt::Debug for ChildGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildGroup")
            .field("os_bound", &self.os_bound())
            .finish()
    }
}

impl Default for ChildGroup {
    fn default() -> Self {
        Self::new()
    }
}

impl ChildGroup {
    /// A new group. On Windows a job object that could not be created leaves the group unbound
    /// ([`ChildGroup::os_bound`] is `false`); the owner's own kill still applies.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: imp::Group::new(),
        }
    }

    /// Puts `child` in the group.
    ///
    /// # Errors
    ///
    /// The OS refused (Windows `AssignProcessToJobObject`).
    pub fn add(&self, child: &Child) -> std::io::Result<()> {
        self.inner.add(child)
    }

    /// Spawns `cmd` already in the group (ADR-7548). On Windows the process is created suspended
    /// (`CREATE_SUSPENDED`; this replaces any creation flags set on `cmd`), assigned to the job,
    /// then resumed, so nothing it starts can run outside the job; a child the job refuses still
    /// runs (the owner's own kill applies, as with [`ChildGroup::add`]), and one that cannot be
    /// resumed is killed and reported. Elsewhere this is `spawn` then [`ChildGroup::add`].
    ///
    /// # Errors
    ///
    /// The spawn failed, or (Windows) the suspended child could not be resumed.
    pub fn spawn(&self, cmd: &mut Command) -> std::io::Result<Spawned> {
        self.inner.spawn(cmd)
    }

    /// Whether the OS ends this group's processes when this process ends: a job object on
    /// Windows, the parent-death signal of [`bind_to_parent`] on Linux, neither elsewhere.
    #[must_use]
    pub fn os_bound(&self) -> bool {
        self.inner.os_bound()
    }
}

/// A child [`ChildGroup::spawn`] started.
#[derive(Debug)]
pub struct Spawned {
    /// The running child.
    pub child: Child,
    /// Why the group could not take it (it runs outside the group), when it could not.
    pub outside: Option<std::io::Error>,
}

/// Sends `SIGKILL` to the process group `pgid` (Unix: `killpg`, no program run, no `PATH` lookup;
/// ADR-7548). A child spawned with `process_group(0)` leads the group whose id is its pid. Pass only
/// the group of a child you own and have not reaped: its pid, and so the group id, cannot be reused
/// before the child is waited for.
///
/// # Errors
///
/// `pgid` 0 or 1 (this process's own group, init) is refused as `InvalidInput`; an id outside
/// `pid_t` too; the OS refused (`ESRCH`: the group is gone). Unsupported elsewhere (Windows ends a
/// process tree with a [`ChildGroup`]).
pub fn kill_process_group(pgid: u32) -> std::io::Result<()> {
    imp_kill::kill_process_group(pgid)
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "android"))]
mod imp_kill {
    pub fn kill_process_group(pgid: u32) -> std::io::Result<()> {
        let group = libc::pid_t::try_from(pgid)
            .ok()
            .filter(|&g| g > 1)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        // SAFETY: `killpg` takes plain integers and touches no memory of this process; a positive
        // group id above 1 never names this process's own group (0) or init.
        if unsafe { libc::killpg(group, libc::SIGKILL) } == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "android")))]
mod imp_kill {
    pub fn kill_process_group(_pgid: u32) -> std::io::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
}

/// `spawn` then [`ChildGroup::add`] (every system but Windows).
#[cfg(not(windows))]
fn spawn_then_add(group: &imp::Group, cmd: &mut Command) -> std::io::Result<Spawned> {
    let child = cmd.spawn()?;
    let outside = group.add(&child).err();
    Ok(Spawned { child, outside })
}

#[cfg(target_os = "linux")]
mod imp {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    pub fn bind_to_parent(cmd: &mut Command) {
        let parent = std::process::id();
        // SAFETY: the closure runs in the child between fork and exec. It calls only `prctl` and
        // `getppid` (async-signal-safe), allocates nothing and takes no lock; the errors it builds
        // from raw OS codes do not allocate either.
        unsafe {
            cmd.pre_exec(move || {
                // Every variadic argument is an unsigned long, as the kernel reads them.
                let signal = libc::SIGKILL as libc::c_ulong;
                let unused: libc::c_ulong = 0;
                if libc::prctl(libc::PR_SET_PDEATHSIG, signal, unused, unused, unused) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent died before `prctl`: the child was already re-parented, so no signal
                // would ever come. Refuse to start instead of running unbound.
                if u32::try_from(libc::getppid()).ok() != Some(parent) {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }

    pub struct Group;

    impl Group {
        pub fn new() -> Self {
            Self
        }

        pub fn add(&self, _child: &Child) -> std::io::Result<()> {
            Ok(())
        }

        pub fn spawn(&self, cmd: &mut Command) -> std::io::Result<super::Spawned> {
            super::spawn_then_add(self, cmd)
        }

        pub fn os_bound(&self) -> bool {
            true
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;
    use std::process::{Child, Command};

    use std::os::windows::process::CommandExt as _;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
    };

    pub fn bind_to_parent(_cmd: &mut Command) {}

    /// The job handle as an integer, so the group is `Send` and `Sync` (the handle is a kernel
    /// object reference, usable from any thread).
    pub struct Group {
        job: Option<isize>,
    }

    impl Group {
        pub fn new() -> Self {
            // SAFETY: plain Win32 calls with valid arguments: no security attributes, no name; the
            // limit structure is a zeroed POD with one flag set, passed with its own size.
            let job = unsafe {
                let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    None
                } else {
                    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                    let ok = SetInformationJobObject(
                        job,
                        JobObjectExtendedLimitInformation,
                        (&raw const info).cast(),
                        u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                            .unwrap_or(u32::MAX),
                    );
                    if ok == 0 {
                        CloseHandle(job);
                        None
                    } else {
                        Some(job as isize)
                    }
                }
            };
            Self { job }
        }

        pub fn add(&self, child: &Child) -> std::io::Result<()> {
            let Some(job) = self.job else {
                return Ok(());
            };
            // SAFETY: `job` is the live job handle this group owns; the child's handle is valid
            // while `child` is borrowed.
            let ok = unsafe { AssignProcessToJobObject(job as HANDLE, child.as_raw_handle() as HANDLE) };
            if ok == 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        pub fn spawn(&self, cmd: &mut Command) -> std::io::Result<super::Spawned> {
            if self.job.is_none() {
                return Ok(super::Spawned {
                    child: cmd.spawn()?,
                    outside: None,
                });
            }
            // Suspended until it is in the job: a process it starts before the assignment would
            // run outside the job and outlive the group (ADR-7548).
            cmd.creation_flags(CREATE_SUSPENDED);
            let mut child = cmd.spawn()?;
            let outside = self.add(&child).err();
            if let Err(e) = resume_process(child.id()) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
            Ok(super::Spawned { child, outside })
        }

        pub fn os_bound(&self) -> bool {
            self.job.is_some()
        }
    }

    /// Resumes every thread of the suspended process `pid` (a process created suspended has one,
    /// its primary thread). The caller owns the unreaped child, so `pid` names it.
    fn resume_process(pid: u32) -> std::io::Result<()> {
        let entry_size = u32::try_from(std::mem::size_of::<THREADENTRY32>()).unwrap_or(u32::MAX);
        let owner_end = std::mem::offset_of!(THREADENTRY32, th32OwnerProcessID) + std::mem::size_of::<u32>();
        let mut resumed = 0_usize;
        let mut error = None;
        // SAFETY: the snapshot is a fresh handle closed once below; the entry is a zeroed POD
        // whose `dwSize` is set before every call, as Thread32First/Next require; each opened
        // thread handle gets one ResumeThread and is closed.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = entry_size;
            let mut more = Thread32First(snapshot, &raw mut entry) != 0;
            while more {
                let complete = usize::try_from(entry.dwSize).is_ok_and(|size| size >= owner_end);
                if complete && entry.th32OwnerProcessID == pid {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if thread.is_null() {
                        error = Some(std::io::Error::last_os_error());
                    } else {
                        if ResumeThread(thread) == u32::MAX {
                            error = Some(std::io::Error::last_os_error());
                        } else {
                            resumed += 1;
                        }
                        CloseHandle(thread);
                    }
                }
                entry.dwSize = entry_size;
                more = Thread32Next(snapshot, &raw mut entry) != 0;
            }
            CloseHandle(snapshot);
        }
        match (resumed, error) {
            (0, Some(e)) => Err(e),
            (0, None) => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the suspended child has no thread to resume",
            )),
            _ => Ok(()),
        }
    }

    impl Drop for Group {
        fn drop(&mut self) {
            if let Some(job) = self.job.take() {
                // SAFETY: the group owns this handle and closes it once; closing ends the job's
                // processes (KILL_ON_JOB_CLOSE), which the owner has already stopped.
                unsafe {
                    CloseHandle(job as HANDLE);
                }
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use std::process::{Child, Command};

    pub fn bind_to_parent(_cmd: &mut Command) {}

    pub struct Group;

    impl Group {
        pub fn new() -> Self {
            Self
        }

        pub fn add(&self, _child: &Child) -> std::io::Result<()> {
            Ok(())
        }

        pub fn spawn(&self, cmd: &mut Command) -> std::io::Result<super::Spawned> {
            super::spawn_then_add(self, cmd)
        }

        pub fn os_bound(&self) -> bool {
            false
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// A bound child starts and runs normally; the binding only adds the death signal.
    #[test]
    fn a_bound_child_runs() {
        let mut cmd = Command::new("true");
        bind_to_parent(&mut cmd);
        let status = cmd.status().expect("run true");
        assert!(status.success());
        let group = ChildGroup::new();
        assert!(group.os_bound());
    }

    /// Whether `pid` is gone or a zombie (not running any more).
    fn not_running(pid: &str) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X')),
        }
    }

    /// `ChildGroup::spawn` runs the child (ADR-7548), and `kill_process_group` ends the group the
    /// child leads, grandchildren included, without running a `kill` program.
    #[test]
    fn a_group_spawned_child_runs_and_its_process_group_is_killed() {
        use std::io::BufRead as _;
        use std::os::unix::process::CommandExt as _;

        let group = ChildGroup::new();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 100000 & echo $!; wait"])
            .stdout(std::process::Stdio::piped())
            .process_group(0);
        let Spawned { mut child, outside } = group.spawn(&mut cmd).expect("spawn sh");
        assert!(outside.is_none(), "{outside:?}");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = std::io::BufReader::new(stdout);
        let mut grandchild = String::new();
        reader.read_line(&mut grandchild).expect("the grandchild's pid");
        let grandchild = grandchild.trim().to_owned();
        assert!(!not_running(&grandchild), "the grandchild runs");

        kill_process_group(child.id()).expect("killpg");
        let status = child.wait().expect("reap");
        assert!(!status.success(), "{status}");
        // The grandchild holds the stdout pipe too: the pipe ends only when it is dead. The
        // channel's bound is a hang guard for a broken kill, never a timing assert.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut rest = Vec::new();
            let _ = std::io::Read::read_to_end(&mut reader, &mut rest);
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(120)).is_ok(),
            "the grandchild in the child's group was killed"
        );
    }

    /// Group 0 (this process's own) and 1 are never signalled.
    #[test]
    fn own_and_init_groups_are_refused() {
        for pgid in [0, 1, u32::MAX] {
            let err = kill_process_group(pgid).expect_err("refused");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{pgid}");
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// A child spawned into the group was created suspended and resumed (ADR-7548): it runs to its
    /// own exit code.
    #[test]
    fn a_suspended_spawn_is_resumed_and_runs() {
        let group = ChildGroup::new();
        assert!(group.os_bound());
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "exit 7"]);
        let Spawned { mut child, outside } = group.spawn(&mut cmd).expect("spawn cmd");
        assert!(outside.is_none(), "{outside:?}");
        assert_eq!(child.wait().expect("wait").code(), Some(7));
    }

    /// Closing the group ends the child and what it started (`ping` under `cmd`): the tree is in
    /// the job from its first instruction. `ping` inherits the stdout pipe, so the pipe ends only
    /// when the grandchild is dead too (a job closed with `KILL_ON_JOB_CLOSE` ends its processes
    /// with exit code 0, so the exit status proves nothing). The channel's bound is a hang guard.
    #[test]
    fn dropping_the_group_ends_a_spawned_tree() {
        use std::io::BufRead as _;

        let group = ChildGroup::new();
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "ping -n 100000 127.0.0.1"])
            .stdout(std::process::Stdio::piped());
        let Spawned { mut child, outside } = group.spawn(&mut cmd).expect("spawn cmd");
        assert!(outside.is_none(), "{outside:?}");
        let mut reader = std::io::BufReader::new(child.stdout.take().expect("piped stdout"));
        // `ping` (the grandchild) is running once it prints its header.
        let mut line = String::new();
        while !line.contains("127.0.0.1") {
            line.clear();
            assert!(
                reader.read_line(&mut line).expect("read") > 0,
                "ping printed nothing"
            );
        }
        drop(group);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut rest = Vec::new();
            let _ = std::io::Read::read_to_end(&mut reader, &mut rest);
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(120)).is_ok(),
            "closing the group ended cmd and the ping it started"
        );
        child.wait().expect("reap");
    }
}

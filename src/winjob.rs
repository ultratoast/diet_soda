//! Contained process trees on Windows via job objects.
//!
//! Windows has no POSIX process group, so a subprocess spawned through the
//! ordinary tokio API is not tied to the harness and its descendants survive a
//! cancelled or dropped run. This module contains the child and every process
//! it creates: the child is spawned `CREATE_SUSPENDED`, assigned to an unnamed
//! job object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and only
//! then resumed. Closing the job handle terminates the whole tree, so dropping
//! [`JobObject`] reaps grandchildren as `killpg(SIGKILL)` does on Unix.
//!
//! # Isolated unsafe invariants
//!
//! All `unsafe` here is confined to raw Win32 calls. The invariants that make
//! each call sound:
//!
//! * Every `HANDLE` returned by a Win32 constructor is owned by an RAII guard
//!   ([`HandleGuard`]) or by [`JobObject`], and is closed exactly once on every
//!   path, including early returns and panics. Borrowed process handles stay
//!   owned by tokio's [`Child`]; they are never closed here.
//! * Values passed by pointer (`JOBOBJECT_EXTENDED_LIMIT_INFORMATION`,
//!   `THREADENTRY32`) are live, `#[repr(C)]`, stack-allocated values that
//!   outlive the call. `THREADENTRY32.dwSize` is set before enumerating.
//! * `AssignProcessToJobObject` is only called with the live process handle of
//!   a child this module spawned and a job handle it created.
//! * `TerminateJobObject` is only called on the live job handle owned by
//!   [`JobObject`]; the handle stays valid across the call and is still closed
//!   exactly once by `Drop`.
//! * `OpenThread` requests only `THREAD_SUSPEND_RESUME` and
//!   `THREAD_QUERY_LIMITED_INFORMATION`; `GetProcessIdOfThread` is called on
//!   that handle to confirm ownership before `ResumeThread`.
//! * Handles are used and closed from a single owner. [`JobObject`] is the one
//!   exception permitted to move between threads: Win32 job handles are
//!   thread-agnostic, and the guard still closes its handle exactly once.
//!
//! # Pre-assignment race elimination
//!
//! A child created with `CREATE_SUSPENDED` executes no user code until its
//! primary thread is resumed. Job assignment therefore completes before the
//! process can spawn children, exec, or touch the filesystem: there is no
//! window in which a fast child escapes into a grandchild outside the job.
//! Only after the assignment succeeds are the child's threads resumed. If any
//! setup or assignment step fails before the resume, the kill-on-close job is
//! closed and [`JobObject::assign`] force-kills the child, so a suspended
//! process is never left behind and an uncontained tree is never run. An
//! `ERROR_ACCESS_DENIED` assignment is a hard failure, not a degradation:
//! containment is the point, so the caller refuses to run uncontained.
//!
//! No breakaway flags (`CREATE_BREAKAWAY_FROM_JOB`,
//! `JOB_OBJECT_LIMIT_BREAKAWAY_OK`) are set. Containment is the point.
#![allow(unsafe_code)]

use anyhow::{anyhow, Context, Result};
use tokio::process::{Child, Command};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_NO_MORE_FILES, FALSE, HANDLE,
        INVALID_HANDLE_VALUE,
    },
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::{
            GetProcessIdOfThread, OpenThread, ResumeThread, CREATE_SUSPENDED,
            THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
        },
    },
};

/// Marks a freshly built command so its child starts suspended.
///
/// Call this after all other command options and immediately before
/// [`tokio::process::Command::spawn`]; the child must be handed to
/// [`JobObject::assign`] before it is resumed. std and tokio expose no getter
/// for the current creation flags and `creation_flags` replaces the flag word,
/// so this sets `CREATE_SUSPENDED` directly. Tokio ORs in
/// `CREATE_UNICODE_ENVIRONMENT` internally. If a caller ever needs additional
/// flags, extend this helper instead of calling `creation_flags` again, which
/// would overwrite the suspension flag.
pub(crate) fn prepare_command(command: &mut Command) {
    command.creation_flags(CREATE_SUSPENDED);
}

/// A Win32 handle this module owns, closed exactly once when dropped.
struct HandleGuard(Option<HANDLE>);

impl HandleGuard {
    /// Takes ownership of `handle`, rejecting the sentinel values the Win32
    /// constructors return to signal failure.
    fn new(handle: HANDLE, what: &str) -> Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(last_error(what));
        }
        Ok(Self(Some(handle)))
    }

    fn get(&self) -> HANDLE {
        // Only `into_inner` clears the slot, and it consumes the guard, so a
        // live guard always holds a handle.
        self.0.expect("handle guard used after release")
    }

    /// Releases ownership of the handle without closing it.
    fn into_inner(mut self) -> HANDLE {
        self.0.take().expect("handle guard used after release")
    }
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            // SAFETY: the guard exclusively owns this handle. The result is
            // discarded because Drop cannot report failure.
            unsafe { CloseHandle(handle) };
        }
    }
}

/// Owns a kill-on-close job object holding a spawned process tree.
///
/// Dropping this value closes the last job handle, which terminates every
/// process still assigned to the job. Keep it alive for as long as the child
/// should be allowed to run.
pub(crate) struct JobObject {
    handle: HANDLE,
}

impl JobObject {
    /// Assigns a child spawned with [`prepare_command`] to a new kill-on-close
    /// job and resumes it.
    ///
    /// The child must have been spawned `CREATE_SUSPENDED`; otherwise it may
    /// have already escaped the job. On success the returned job owns the
    /// process tree. Any assignment failure, including `ERROR_ACCESS_DENIED`,
    /// is a hard error: containment is the point, so the caller refuses to run
    /// uncontained. On every error path the kill-on-close job is closed and the
    /// suspended child is force-killed, so no suspended process is left behind.
    pub(crate) fn assign(child: &mut Child) -> Result<Self> {
        match Self::setup_and_assign(child) {
            Ok(job) => Ok(job),
            Err(error) => {
                // A failure can leave the CREATE_SUSPENDED child alive but never
                // scheduled. If it was assigned before the failure, closing the
                // kill-on-close job in `setup_and_assign` terminates it; a child
                // that was never assigned would otherwise survive. Force it
                // down so this function never returns with a suspended process.
                let _ = child.start_kill();
                Err(error)
            }
        }
    }

    fn setup_and_assign(child: &mut Child) -> Result<Self> {
        let job = HandleGuard::new(
            // SAFETY: a null `SECURITY_ATTRIBUTES` and a null name request an
            // unnamed job owned by the returned handle.
            unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) },
            "CreateJobObject",
        )?;

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` is a live `repr(C)` value, the class selector matches
        // its type, and the length is its exact size.
        let configured = unsafe {
            SetInformationJobObject(
                job.get(),
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(last_error("SetInformationJobObject"));
        }

        let process = child
            .raw_handle()
            .ok_or_else(|| anyhow!("child exited before it could be assigned to a job"))?;
        // SAFETY: `process` is the live process handle of `child`, `job` is the
        // job handle created above, and both remain valid across the call.
        if unsafe { AssignProcessToJobObject(job.get(), process) } == 0 {
            let code = unsafe { GetLastError() };
            if code == ERROR_ACCESS_DENIED {
                // Containment is impossible (a pre-existing non-breakaway job,
                // or a platform without nested jobs). Refuse to run the tree
                // uncontained: the `job` guard drops to close the handle and
                // `assign` force-kills the still-suspended child.
                return Err(anyhow!(
                    "AssignProcessToJobObject denied (Win32 error {code}): cannot contain the subprocess tree in a job object"
                ));
            }
            return Err(anyhow!(
                "AssignProcessToJobObject failed (Win32 error {code})"
            ));
        }

        // The process is in the kill-on-close job now. If resuming fails, the
        // `job` guard drops and closing the handle terminates the tree, and the
        // `assign` wrapper force-kills as a backstop.
        Self::resume_threads(child).context("resuming a suspended child after job assignment")?;

        Ok(Self {
            handle: job.into_inner(),
        })
    }

    /// Resumes every suspended thread belonging to `child`.
    ///
    /// A freshly created process has one primary thread; enumerating is done
    /// through the system thread snapshot rather than a per-process API because
    /// ToolHelp is the supported way to obtain thread IDs for another process.
    /// Any thread handle opened here is closed before returning.
    fn resume_threads(child: &mut Child) -> Result<()> {
        let Some(pid) = child.id() else {
            // The child already exited, so no suspended thread remains.
            return Ok(());
        };
        // SNAPTHREAD snapshots every thread on the host; its process-ID argument
        // is ignored, so the owner is filtered below.
        let snapshot = HandleGuard::new(
            // SAFETY: a thread snapshot takes no pointers and returns an owned
            // handle, guarded for the rest of this function.
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) },
            "CreateToolhelp32Snapshot",
        )?;

        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..THREADENTRY32::default()
        };
        // SAFETY: `entry` is a live `repr(C)` value with `dwSize` set and
        // `snapshot` is a valid handle.
        if unsafe { Thread32First(snapshot.get(), &mut entry) } == 0 {
            // An empty snapshot is the only benign failure; any other error
            // means enumeration could not start and the suspended child would
            // otherwise be left unresumed.
            let code = unsafe { GetLastError() };
            if code == ERROR_NO_MORE_FILES {
                return Ok(());
            }
            return Err(anyhow!("Thread32First failed (Win32 error {code})"));
        }
        loop {
            if entry.th32OwnerProcessID == pid {
                let thread = HandleGuard::new(
                    // SAFETY: `entry.th32ThreadID` names a thread reported by
                    // the snapshot and `FALSE` requests a non-inheritable
                    // handle. THREAD_QUERY_LIMITED_INFORMATION is required by
                    // `GetProcessIdOfThread` below.
                    unsafe {
                        OpenThread(
                            THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                            FALSE,
                            entry.th32ThreadID,
                        )
                    },
                    "OpenThread",
                )?;
                // The snapshot's thread ID may have been recycled by another
                // process between the snapshot and `OpenThread`. Confirm the
                // opened thread still belongs to `pid`; on a mismatch the guard
                // drops, closing the handle, and the unrelated thread is never
                // touched.
                // SAFETY: `thread` grants THREAD_QUERY_LIMITED_INFORMATION, the
                // access `GetProcessIdOfThread` requires, on a live thread.
                if unsafe { GetProcessIdOfThread(thread.get()) } == pid {
                    // SAFETY: `thread` grants THREAD_SUSPEND_RESUME on a thread
                    // confirmed to belong to the child; `u32::MAX` is the
                    // documented failure sentinel.
                    if unsafe { ResumeThread(thread.get()) } == u32::MAX {
                        return Err(last_error("ResumeThread"));
                    }
                }
            }
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            // SAFETY: `entry` and `snapshot` remain valid; the call advances the
            // enumeration.
            if unsafe { Thread32Next(snapshot.get(), &mut entry) } == 0 {
                break;
            }
        }
        Ok(())
    }

    /// Terminates every process in the job with `TerminateJobObject`.
    ///
    /// Unlike dropping the guard, this does not consume or close the job
    /// handle: the job stays valid and `Drop` still closes it exactly once.
    /// Callers use this for explicit tree teardown when they must keep the
    /// guard alive afterwards.
    pub(crate) fn terminate(&self) -> Result<()> {
        // SAFETY: `self.handle` is the live job handle owned exclusively by
        // this value and remains valid across the call; exit code 1 marks the
        // terminated processes. The handle is not closed here.
        if unsafe { TerminateJobObject(self.handle, 1) } == 0 {
            return Err(last_error("TerminateJobObject"));
        }
        Ok(())
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // Closing the last handle to a job configured with KILL_ON_JOB_CLOSE
        // terminates every process still in it, tearing down the tree.
        // SAFETY: `self.handle` is a job handle owned exclusively by this value.
        unsafe { CloseHandle(self.handle) };
    }
}

// SAFETY: a Win32 `HANDLE` is a thread-agnostic kernel object reference. A job
// handle may be closed from any thread, and `JobObject` closes it exactly once
// in `Drop`, so moving the guard across threads cannot double-close it or race
// another use. The unsafe `Send` is needed because `process::run` holds the
// guard across awaits in a future that must itself be `Send`.
unsafe impl Send for JobObject {}

/// Builds an error from the calling thread's last Win32 error code.
fn last_error(what: &str) -> anyhow::Error {
    // SAFETY: `GetLastError` takes no arguments and reads thread-local state.
    let code = unsafe { GetLastError() };
    anyhow!("{what} failed (Win32 error {code})")
}

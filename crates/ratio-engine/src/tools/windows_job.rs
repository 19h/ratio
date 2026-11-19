use std::{
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::null,
};

use anyhow::{Context, Result, ensure};
use tokio::process::{Child, Command};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
        Threading::{
            CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, OpenThread, ResumeThread,
            THREAD_SUSPEND_RESUME,
        },
    },
};

pub(super) struct Job(OwnedHandle);

impl Job {
    pub(super) fn prepare(command: &mut Command) -> Result<Self> {
        // SAFETY: both optional arguments are null. The returned handle is new.
        let job =
            owned(unsafe { CreateJobObjectW(null(), null()) }).context("create command job")?;
        let job = Self(job);
        job.set_limits(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)?;
        // The primary thread cannot start a child before it joins the job.
        command.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP);
        Ok(job)
    }

    pub(super) fn disarm(&self) -> Result<()> {
        self.set_limits(0)
    }

    fn set_limits(&self, flags: u32) -> Result<()> {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = flags;
        // SAFETY: the structure and size match the information class.
        if unsafe {
            SetInformationJobObject(
                self.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).context("set command job limits");
        }
        Ok(())
    }

    pub(super) fn start(&self, child: &Child) -> Result<()> {
        let process = child.raw_handle().context("get suspended process handle")?;
        let pid = child.id().context("get suspended process ID")?;
        // SAFETY: child and self retain their live handles across the call.
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), process) } == 0 {
            return Err(io::Error::last_os_error())
                .context("assign suspended process to command job");
        }
        let thread = primary_thread(pid)?;
        // SAFETY: the handle was opened with THREAD_SUSPEND_RESUME.
        let previous = unsafe { ResumeThread(thread.as_raw_handle()) };
        if previous == u32::MAX {
            return Err(io::Error::last_os_error()).context("resume command process");
        }
        ensure!(previous == 1, "unexpected suspend count on command process");
        Ok(())
    }
}

fn primary_thread(pid: u32) -> Result<OwnedHandle> {
    // std::process retains the process handle and closes its primary-thread
    // handle. The suspended process has not run, so locate its primary thread.
    // SAFETY: TH32CS_SNAPTHREAD snapshots all threads and ignores its PID argument.
    let snapshot = owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })
        .context("snapshot threads for suspended command")?;
    let size = size_of::<THREADENTRY32>() as u32;
    let mut entry = THREADENTRY32 {
        dwSize: size,
        ..Default::default()
    };
    // SAFETY: entry is writable and initialized with its structure size.
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while found != 0 {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: OpenThread returns a new handle with the requested access.
            return owned(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) })
                .context("open primary thread of suspended command");
        }
        entry.dwSize = size;
        // SAFETY: snapshot remains live and entry has been sized for the call.
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    Err(io::Error::last_os_error()).context("locate primary thread of suspended command")
}

fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: callers pass only newly acquired, owned Win32 handles.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

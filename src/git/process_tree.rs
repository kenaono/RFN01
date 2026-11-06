//! Keep a Git command and the processes it creates under the same Windows job.

use std::io;
use std::process::{Child, Command};

pub(crate) struct ManagedChild {
    pub(crate) process: Child,
    #[cfg(windows)]
    job: Option<std::os::windows::io::OwnedHandle>,
}

impl ManagedChild {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(windows)]
        {
            windows_job::spawn(command)
        }
        #[cfg(not(windows))]
        {
            Ok(Self {
                process: command.spawn()?,
            })
        }
    }

    pub(crate) fn close_tree(&mut self) {
        #[cfg(windows)]
        drop(self.job.take());
    }

    pub(crate) fn stop(&mut self) {
        self.close_tree();
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        self.close_tree();
    }
}

#[cfg(windows)]
mod windows_job {
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
    };
    use windows::core::PCWSTR;

    pub(super) fn spawn(command: &mut Command) -> io::Result<ManagedChild> {
        // An unnamed, non-inheritable job dies with the owning process as well.
        let job =
            own(unsafe { CreateJobObjectW(None, PCWSTR::null()) }).map_err(io::Error::other)?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                HANDLE(job.as_raw_handle()),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(io::Error::other)?;

        // Git cannot run a helper before assignment. std's stable Child API
        // exposes only the process handle, so find the suspended initial thread.
        command.creation_flags(CREATE_NO_WINDOW.0 | CREATE_SUSPENDED.0);
        let process = command.spawn()?;
        let mut child = ManagedChild {
            process,
            job: Some(job),
        };
        let job = child.job.as_ref().expect("new Git job");
        let start = unsafe {
            AssignProcessToJobObject(
                HANDLE(job.as_raw_handle()),
                HANDLE(child.process.as_raw_handle()),
            )
        }
        .and_then(|()| resume_initial_thread(child.process.id()));
        if let Err(error) = start {
            child.stop();
            return Err(io::Error::other(error));
        }
        Ok(child)
    }

    fn own(result: windows::core::Result<HANDLE>) -> windows::core::Result<OwnedHandle> {
        result.map(|handle| {
            // These APIs return a newly owned kernel handle on success.
            unsafe { OwnedHandle::from_raw_handle(handle.0) }
        })
    }

    fn resume_initial_thread(pid: u32) -> windows::core::Result<()> {
        let snapshot = own(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
        let snapshot_handle = HANDLE(snapshot.as_raw_handle());
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        unsafe { Thread32First(snapshot_handle, &mut entry)? };
        while entry.th32OwnerProcessID != pid {
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            unsafe { Thread32Next(snapshot_handle, &mut entry)? };
        }
        let thread = own(unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) })?;
        if unsafe { ResumeThread(HANDLE(thread.as_raw_handle())) } == u32::MAX {
            return Err(windows::core::Error::from_thread());
        }
        Ok(())
    }
}

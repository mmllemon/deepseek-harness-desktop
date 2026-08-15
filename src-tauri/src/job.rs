//! Windows 作业对象（Job Object）——进程树回收的 defense-in-depth 兜底（§4.1 / §8 / §13.8 D8）。
//!
//! 优先依赖 `tauri-plugin-shell` 的进程生命周期管理；仅当其未能回收 `dsh` 派生的
//! PowerShell 子进程时，才用 Job Object 把整棵进程树一并强终止，杜绝孤儿 RCE 面。
//! 非 Windows 平台本模块为空实现，由 Tauri 默认机制承担。

#[cfg(windows)]
pub mod job {
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_ALL_ACCESS};

    pub struct JobHandle(pub HANDLE);

    impl JobHandle {
        /// 创建带 `KILL_ON_JOB_CLOSE` 的作业：句柄关闭时整树被内核回收。
        pub fn new_with_kill_on_close() -> Option<JobHandle> {
            unsafe {
                let h = CreateJobObjectW(null_mut(), null_mut());
                if h == INVALID_HANDLE_VALUE {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *mut _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    CloseHandle(h);
                    return None;
                }
                Some(JobHandle(h))
            }
        }

        /// 把指定 PID 纳入作业（从而纳入进程树回收范围）。
        pub fn assign(&self, pid: u32) -> bool {
            unsafe {
                let ph = OpenProcess(PROCESS_ALL_ACCESS, 0, pid);
                if ph == INVALID_HANDLE_VALUE {
                    return false;
                }
                let r = AssignProcessToJobObject(self.0, ph);
                CloseHandle(ph);
                r != 0
            }
        }

        /// 立即终止作业内全部进程（含派生子进程）。
        pub fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.0, 0);
            }
        }
    }

    impl Drop for JobHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(not(windows))]
pub mod job {
    /// 非 Windows：无 Job Object，由 Tauri 默认进程管理承担。
    pub struct JobHandle;

    impl JobHandle {
        pub fn new_with_kill_on_close() -> Option<JobHandle> {
            None
        }
        pub fn assign(&self, _pid: u32) -> bool {
            true
        }
        pub fn terminate(&self) {}
    }
}

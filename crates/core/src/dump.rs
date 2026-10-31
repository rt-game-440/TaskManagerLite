//! 创建进程转储文件（#75，`MiniDumpWriteDump`）。
//!
//! 核心库不返回显示文案：失败原因用 [`DumpError`]，中文文案在界面层 `texts.rs`。

use std::path::Path;

use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_NONE,
};
use windows::Win32::System::Diagnostics::Debug::{
    MINIDUMP_TYPE, MiniDumpNormal, MiniDumpWithFullMemory, MiniDumpWithHandleData, MiniDumpWithThreadInfo,
    MiniDumpWithUnloadedModules, MiniDumpWriteDump,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::SystemInformation::IMAGE_FILE_MACHINE;
use windows::Win32::System::Threading::{IsWow64Process2, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};
use windows::core::HSTRING;

use crate::control::OwnedHandle;
use crate::error::SystemError;
use crate::procctl::{ProcessControlError, eco_protected_pid, open_checked};
use crate::snapshot::ProcessKey;

/// 转储类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpKind {
    /// 最小：线程、模块、堆栈，体积小。
    Mini,
    /// 完整：包含进程全部可访问内存，体积接近进程的私有内存。
    Full,
}

impl DumpKind {
    fn flags(self) -> MINIDUMP_TYPE {
        match self {
            Self::Mini => MiniDumpNormal,
            Self::Full => MINIDUMP_TYPE(
                MiniDumpWithFullMemory.0
                    | MiniDumpWithHandleData.0
                    | MiniDumpWithThreadInfo.0
                    | MiniDumpWithUnloadedModules.0,
            ),
        }
    }
}

/// 创建转储失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DumpError {
    /// 进程已退出，或 PID 已被别的进程复用。
    Gone,
    /// 系统关键进程（PID 0、4）或本程序自身，不允许转储。
    Protected,
    /// 拒绝访问：受保护进程、他人的进程或需要提权。
    AccessDenied,
    /// 位数不匹配：本程序是 64 位而目标是 32 位（WoW64）进程，系统无法生成转储。
    Bitness,
    /// 无法创建目标文件（路径无效、没有权限等）。
    CreateFile(SystemError),
    /// 写入过程中失败（磁盘空间不足等），已删除不完整的文件。
    Write(SystemError),
    /// 其余系统调用失败。
    System(SystemError),
}

impl DumpError {
    /// 写入日志用的错误码；与系统错误码无关的原因为 0。
    pub fn code(&self) -> i32 {
        match self {
            Self::Gone | Self::Protected | Self::Bitness => 0,
            Self::AccessDenied => ERROR_ACCESS_DENIED.0 as i32,
            Self::CreateFile(e) | Self::Write(e) | Self::System(e) => match e {
                SystemError::Win32(c) => *c,
                SystemError::Other { code, .. } => *code,
            },
        }
    }
}

impl std::fmt::Display for DumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone => f.write_str("process is gone"),
            Self::Protected => f.write_str("protected process"),
            Self::AccessDenied => f.write_str("access denied"),
            Self::Bitness => f.write_str("dump of a 32-bit process from a 64-bit build is not supported"),
            Self::CreateFile(e) => write!(f, "cannot create dump file: {e}"),
            Self::Write(e) => write!(f, "cannot write dump: {e}"),
            Self::System(e) => write!(f, "dump failed: {e}"),
        }
    }
}

impl std::error::Error for DumpError {}

impl From<ProcessControlError> for DumpError {
    fn from(e: ProcessControlError) -> Self {
        match e {
            ProcessControlError::Gone => Self::Gone,
            ProcessControlError::AccessDenied => Self::AccessDenied,
            ProcessControlError::Protected => Self::Protected,
            ProcessControlError::System(s) => Self::System(s),
            _ => Self::System(SystemError::Win32(0)),
        }
    }
}

fn open_for_dump(key: ProcessKey) -> Result<OwnedHandle, DumpError> {
    if eco_protected_pid(key.pid, std::process::id()) {
        return Err(DumpError::Protected);
    }
    Ok(open_checked(key, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)?)
}

/// 本程序是 64 位而目标是 32 位进程。
fn bitness_mismatch(h: &OwnedHandle) -> bool {
    if !cfg!(target_pointer_width = "64") {
        return false;
    }
    let (mut process, mut native) = (IMAGE_FILE_MACHINE(0), IMAGE_FILE_MACHINE(0));
    unsafe { IsWow64Process2(h.raw(), &mut process, Some(&mut native)) }.is_ok() && process.0 != 0
}

/// 能否对这个进程创建转储：返回预知的失败原因（用于菜单置灰与提示）。
pub fn can_dump(key: ProcessKey) -> Result<(), DumpError> {
    let h = open_for_dump(key)?;
    if bitness_mismatch(&h) { Err(DumpError::Bitness) } else { Ok(()) }
}

/// 完整转储的估算体积（字节）：进程的私有提交内存；读不到时为 `None`。
pub fn estimate_full_size(key: ProcessKey) -> Option<u64> {
    let h = open_for_dump(key).ok()?;
    let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    unsafe { GetProcessMemoryInfo(h.raw(), &mut c as *mut _ as *mut _, c.cb) }.ok()?;
    Some(c.PrivateUsage as u64)
}

/// 把 `key` 的转储写到 `path`。失败（含写到一半磁盘已满）时删除不完整的文件，不留空文件。
///
/// 阻塞调用，完整转储可能耗时较长，请在后台线程调用。
pub fn write_dump(key: ProcessKey, path: &Path, kind: DumpKind) -> Result<(), DumpError> {
    let proc = open_for_dump(key)?;
    if bitness_mismatch(&proc) {
        return Err(DumpError::Bitness);
    }
    let file = unsafe {
        CreateFileW(
            &HSTRING::from(path.as_os_str()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map(OwnedHandle::from_raw)
    .map_err(|e| DumpError::CreateFile(SystemError::from_windows(&e)))?;
    let result = unsafe { MiniDumpWriteDump(proc.raw(), key.pid, file.raw(), kind.flags(), None, None, None) };
    drop(file);
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(path);
            let raw = e.code().0 as u32;
            // 0x8007012B = ERROR_PARTIAL_COPY：目标进程内存读不全（受保护等）。
            if raw == 0x8007_012B {
                return Err(DumpError::AccessDenied);
            }
            if raw >> 16 == 0x8007 && (raw & 0xFFFF) as i32 == ERROR_ACCESS_DENIED.0 as i32 {
                return Err(DumpError::AccessDenied);
            }
            Err(DumpError::Write(SystemError::from_windows(&e)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(pid: u32) -> ProcessKey {
        let h = crate::control::open(pid, PROCESS_QUERY_INFORMATION).unwrap();
        ProcessKey { pid, create_time: crate::control::create_time(&h).unwrap() }
    }

    #[test]
    fn refuses_system_and_self() {
        assert_eq!(can_dump(ProcessKey { pid: 4, create_time: 0 }), Err(DumpError::Protected));
        assert_eq!(can_dump(key_of(std::process::id())), Err(DumpError::Protected));
    }

    #[test]
    fn dumps_a_child_and_cleans_up_on_failure() {
        let mut child = std::process::Command::new("ping")
            .args(["-n", "20", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let key = key_of(child.id());
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tml-dump-test.{}.dmp", key.pid));
        let r = write_dump(key, &path, DumpKind::Mini);
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let bad = write_dump(key, &dir.join("no-such-dir-tml").join("x.dmp"), DumpKind::Mini);
        let est = estimate_full_size(key);
        let _ = child.kill();
        let _ = std::fs::remove_file(&path);
        assert!(r.is_ok(), "{r:?}");
        assert!(size > 1000);
        assert!(matches!(bad, Err(DumpError::CreateFile(_))));
        assert!(est.is_some_and(|n| n > 0));
    }
}

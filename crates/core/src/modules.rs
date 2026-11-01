//! 进程已加载模块（#74，进程详情窗口“模块”页签）。
//!
//! 用 `K32EnumProcessModulesEx(LIST_MODULES_ALL)` 枚举：32 位目标进程会同时列出 32 位与 64 位模块。
//! 核心库不返回显示文案：失败原因用 [`ModuleError`]，文案在界面层 `texts.rs`。

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PARTIAL_COPY, HMODULE};
use windows::Win32::System::ProcessStatus::{
    GetModuleFileNameExW, GetModuleInformation, K32EnumProcessModulesEx, LIST_MODULES_ALL, MODULEINFO,
};
use windows::Win32::System::Threading::{PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};

use crate::error::SystemError;
use crate::procctl::{ProcessControlError, open_checked};
use crate::snapshot::ProcessKey;

/// 一个已加载的模块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    /// 文件名（路径最后一段）。
    pub name: String,
    pub path: PathBuf,
    /// 加载基址。
    pub base: u64,
    /// 映像大小（字节）。
    pub size: u32,
}

/// 读取模块列表失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleError {
    /// 进程已退出，或 PID 已被别的进程复用。
    Gone,
    /// PID 0、4：没有用户态模块。
    Protected,
    /// 拒绝访问：受保护进程、他人的进程或需要提权。
    AccessDenied,
    /// 目标正在启动或退出，模块表暂时读不到，下次刷新再试。
    NotReady,
    System(SystemError),
}

impl ModuleError {
    /// 日志用错误码；与系统错误码无关的原因为 0。
    pub fn code(&self) -> i32 {
        match self {
            Self::AccessDenied => ERROR_ACCESS_DENIED.0 as i32,
            Self::NotReady => ERROR_PARTIAL_COPY.0 as i32,
            Self::System(SystemError::Win32(c)) => *c,
            Self::System(SystemError::Other { code, .. }) => *code,
            _ => 0,
        }
    }

    /// 提权后可能读到（界面据此提示“以管理员身份重新启动”）。
    pub fn needs_elevation(&self) -> bool {
        matches!(self, Self::AccessDenied)
    }
}

impl std::fmt::Display for ModuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone => f.write_str("process is gone"),
            Self::Protected => f.write_str("no user-mode modules"),
            Self::AccessDenied => f.write_str("access denied"),
            Self::NotReady => f.write_str("module table not ready"),
            Self::System(e) => write!(f, "cannot list modules: {e}"),
        }
    }
}

impl std::error::Error for ModuleError {}

impl From<ProcessControlError> for ModuleError {
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

/// 列出进程已加载的模块，按基址升序。
pub fn list_modules(key: ProcessKey) -> Result<Vec<Module>, ModuleError> {
    if key.pid <= 4 {
        return Err(ModuleError::Protected);
    }
    let process = open_checked(key, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)?;
    let handle = process.raw();
    let mut mods: Vec<HMODULE> = vec![HMODULE::default(); 256];
    // 模块数可能在两次调用之间增长：按返回的字节数扩容后重试。
    for _ in 0..6 {
        let mut needed = 0u32;
        let bytes = (mods.len() * std::mem::size_of::<HMODULE>()) as u32;
        // SAFETY: handle 有效；缓冲区长度与 bytes 一致。
        let r = unsafe { K32EnumProcessModulesEx(handle, mods.as_mut_ptr(), bytes, &mut needed, LIST_MODULES_ALL.0) };
        if !r.as_bool() {
            // SAFETY: 紧接失败调用读取线程最后错误。
            let code = unsafe { windows::Win32::Foundation::GetLastError() }.0;
            return Err(match code {
                5 => ModuleError::AccessDenied,
                299 => ModuleError::NotReady,
                _ => ModuleError::System(SystemError::Win32(code as i32)),
            });
        }
        if needed <= bytes {
            mods.truncate(needed as usize / std::mem::size_of::<HMODULE>());
            let mut out: Vec<Module> = mods
                .iter()
                .filter_map(|&m| {
                    let mut buf = [0u16; 1024];
                    // SAFETY: 缓冲区有效；失败返回 0。
                    let n = unsafe { GetModuleFileNameExW(Some(handle), Some(m), &mut buf) } as usize;
                    if n == 0 {
                        return None;
                    }
                    let path = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
                    let mut info = MODULEINFO::default();
                    // SAFETY: info 大小与传入一致。
                    let ok = unsafe {
                        GetModuleInformation(handle, m, &mut info, std::mem::size_of::<MODULEINFO>() as u32)
                    }
                    .is_ok();
                    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    Some(Module { name, path, base: m.0 as usize as u64, size: if ok { info.SizeOfImage } else { 0 } })
                })
                .collect();
            out.sort_by_key(|m| m.base);
            return Ok(out);
        }
        mods.resize(needed as usize / std::mem::size_of::<HMODULE>() + 16, HMODULE::default());
    }
    Err(ModuleError::NotReady)
}

/// 文件版本信息里的（文件说明, 公司名称）。
pub fn file_version_strings(path: &Path) -> (Option<String>, Option<String>) {
    crate::details::version_strings(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> ProcessKey {
        let pid = std::process::id();
        let h = crate::control::open(pid, PROCESS_QUERY_INFORMATION).unwrap();
        ProcessKey { pid, create_time: crate::control::create_time(&h).unwrap() }
    }

    #[test]
    fn lists_own_modules_including_exe_and_ntdll() {
        let mods = list_modules(me()).expect("读取自身模块");
        assert!(mods.iter().any(|m| m.name.eq_ignore_ascii_case("ntdll.dll")));
        assert!(mods.iter().any(|m| m.name.to_ascii_lowercase().contains("tml_core") || m.name.ends_with(".exe")));
        assert!(mods.windows(2).all(|w| w[0].base <= w[1].base));
        assert!(mods.iter().all(|m| m.size > 0 && m.base != 0));
    }

    #[test]
    fn system_pids_have_no_modules() {
        let k = ProcessKey { pid: 4, create_time: 0 };
        assert_eq!(list_modules(k), Err(ModuleError::Protected));
    }

    #[test]
    fn version_strings_of_ntdll() {
        let (d, c) = file_version_strings(Path::new(r"C:\Windows\System32\ntdll.dll"));
        assert!(d.is_some() && c.is_some());
    }
}

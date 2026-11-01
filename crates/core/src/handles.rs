//! 进程打开的句柄（#91，进程详情窗口“句柄”页签）。
//!
//! 列表用 `NtQueryInformationProcess(ProcessHandleInformation)`（Win8+）一次取回句柄值、类型索引与访问掩码，
//! 类型名按索引缓存（类型索引在一次开机内全系统一致）：复制目标进程里该类型的任一句柄，再 `NtQueryObject(ObjectTypeInformation)`。
//! 名称要复制句柄后 `NtQueryObject(ObjectNameInformation)`，对命名管道等同步文件对象可能永远不返回，
//! 所以放在一次性辅助线程里带超时等待，超时就放弃并报告 [`NameResult::TimedOut`]（辅助线程会泄漏到调用返回为止）；
//! 已知会卡住的“文件句柄 + 同步访问掩码”组合直接跳过，不去试。
//! 核心库不返回显示文案：失败原因用 [`HandleError`]，文案在界面层 `texts.rs`。

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
use windows::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, NTSTATUS};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::{GetCurrentProcess, PROCESS_DUP_HANDLE, PROCESS_QUERY_INFORMATION};
use windows::core::w;

use crate::control::OwnedHandle;
use crate::error::SystemError;
use crate::procctl::{ProcessControlError, open_checked};
use crate::snapshot::ProcessKey;

/// `PROCESSINFOCLASS::ProcessHandleInformation`。
const PROCESS_HANDLE_INFORMATION: PROCESSINFOCLASS = PROCESSINFOCLASS(51);
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;
const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023u32 as i32;
/// 同一类型最多试几次去解析类型名（复制偶尔会被拒绝）。
const TYPE_ATTEMPTS: u32 = 4;
/// 超时放弃的次数达到这个数后，不再给文件句柄查名称（每次超时都会泄漏一个卡在系统调用里的线程）。
const MAX_STUCK: u32 = 8;

/// 一个句柄。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    /// 句柄值（进程内的数字）。
    pub value: u64,
    pub type_index: u32,
    /// 类型名（`File`、`Key`、`Event`…）；暂时解析不出来时为空。
    pub type_name: String,
    /// 授予的访问掩码。
    pub access: u32,
}

/// 读取句柄失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleError {
    /// 进程已退出，或 PID 已被别的进程复用。
    Gone,
    /// PID 0、4：系统进程，不提供句柄。
    Protected,
    /// 拒绝访问：受保护进程、他人的进程或需要提权。
    AccessDenied,
    System(SystemError),
}

impl HandleError {
    /// 日志用错误码；与系统错误码无关的原因为 0。
    pub fn code(&self) -> i32 {
        match self {
            Self::AccessDenied => 5,
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

impl std::fmt::Display for HandleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone => f.write_str("process is gone"),
            Self::Protected => f.write_str("system process"),
            Self::AccessDenied => f.write_str("access denied"),
            Self::System(e) => write!(f, "cannot list handles: {e}"),
        }
    }
}

impl std::error::Error for HandleError {}

impl From<ProcessControlError> for HandleError {
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

/// 查询一个句柄名称的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameResult {
    Name(String),
    /// 对象没有名称（匿名事件、线程等）。
    Unnamed,
    /// 查询卡住，超时放弃（或属于已知会卡住的组合而被跳过）。
    TimedOut,
    /// 复制或查询失败。
    Failed,
}

#[repr(C)]
struct SnapshotHeader {
    count: usize,
    reserved: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SnapshotEntry {
    value: usize,
    handle_count: usize,
    pointer_count: usize,
    access: u32,
    type_index: u32,
    attributes: u32,
    reserved: u32,
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum: u16,
    buffer: *const u16,
}

type NtQueryObject = unsafe extern "system" fn(
    handle: HANDLE,
    class: i32,
    info: *mut c_void,
    len: u32,
    returned: *mut u32,
) -> NTSTATUS;

fn nt_query_object() -> Option<NtQueryObject> {
    static F: OnceLock<Option<NtQueryObject>> = OnceLock::new();
    *F.get_or_init(|| unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let f = GetProcAddress(ntdll, windows::core::s!("NtQueryObject"))?;
        // SAFETY: 导出函数的签名即上面的类型。
        Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, NtQueryObject>(f))
    })
}

fn type_cache() -> &'static Mutex<HashMap<u32, String>> {
    static C: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// 把目标进程里的句柄复制到本进程（同样的访问权限）。
fn duplicate(process: HANDLE, value: u64) -> Option<HANDLE> {
    let mut dup = HANDLE::default();
    // SAFETY: process 是带 PROCESS_DUP_HANDLE 的有效句柄；dup 是有效的输出位置。
    unsafe {
        DuplicateHandle(
            process,
            HANDLE(value as usize as *mut c_void),
            GetCurrentProcess(),
            &mut dup,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
    }
    .ok()?;
    Some(dup)
}

/// 读 `NtQueryObject` 返回的开头那个 `UNICODE_STRING`（类型信息与名称信息的第一个字段都是它）。
fn query_object_string(h: HANDLE, class: i32) -> Option<String> {
    let query = nt_query_object()?;
    let mut buf = vec![0u8; 1024];
    for _ in 0..3 {
        let mut needed = 0u32;
        // SAFETY: buf 足够大且按 8 字节对齐使用（Vec<u8> 在分配器下至少 8 对齐，下面读取用 read_unaligned 兜底）。
        let status = unsafe { query(h, class, buf.as_mut_ptr().cast(), buf.len() as u32, &mut needed) };
        if status.0 == STATUS_INFO_LENGTH_MISMATCH || status.0 == STATUS_BUFFER_OVERFLOW || status.0 == STATUS_BUFFER_TOO_SMALL {
            buf.resize((needed as usize).max(buf.len() * 2), 0);
            continue;
        }
        if status.0 < 0 {
            return None;
        }
        // SAFETY: 成功时缓冲区开头是 UNICODE_STRING，其 Buffer 指向同一缓冲区内的字符。
        let us = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<UnicodeString>()) };
        if us.length == 0 || us.buffer.is_null() {
            return Some(String::new());
        }
        let chars = unsafe { std::slice::from_raw_parts(us.buffer, us.length as usize / 2) };
        return Some(String::from_utf16_lossy(chars));
    }
    None
}

/// 列出进程的句柄，按句柄值升序。类型名按索引缓存，缺的类型会复制一个该类型的句柄来解析。
pub fn list_handles(key: ProcessKey) -> Result<Vec<Handle>, HandleError> {
    if key.pid <= 4 {
        return Err(HandleError::Protected);
    }
    let process = open_checked(key, PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE)?;
    let mut buf = vec![0u8; 256 * 1024];
    let entries: Vec<SnapshotEntry> = loop {
        let mut needed = 0u32;
        // SAFETY: process 有效；缓冲区长度与传入一致。
        let status = unsafe {
            NtQueryInformationProcess(
                process.raw(),
                PROCESS_HANDLE_INFORMATION,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut needed,
            )
        };
        if status.0 == STATUS_INFO_LENGTH_MISMATCH {
            // 句柄数在两次调用之间可能还在增长：多留一些余量。
            buf.resize((needed as usize).max(buf.len()) + 64 * 1024, 0);
            continue;
        }
        if status.is_err() {
            return Err(match status.0 as u32 {
                0xC000_0022 => HandleError::AccessDenied,
                0xC000_000B | 0xC000_0008 => HandleError::Gone,
                n => HandleError::System(SystemError::Win32(n as i32)),
            });
        }
        // SAFETY: 成功时缓冲区开头是 header，后跟 count 个 entry；用 read_unaligned 读取。
        let head = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<SnapshotHeader>()) };
        let base = std::mem::size_of::<SnapshotHeader>();
        let n = head.count.min((buf.len() - base) / std::mem::size_of::<SnapshotEntry>());
        break (0..n)
            .map(|i| unsafe {
                std::ptr::read_unaligned(buf.as_ptr().add(base + i * std::mem::size_of::<SnapshotEntry>()).cast::<SnapshotEntry>())
            })
            .collect();
    };
    let mut attempts: HashMap<u32, u32> = HashMap::new();
    let mut out: Vec<Handle> = entries
        .iter()
        .map(|e| {
            let cached = type_cache().lock().ok().and_then(|c| c.get(&e.type_index).cloned());
            let type_name = cached.unwrap_or_else(|| {
                let tries = attempts.entry(e.type_index).or_insert(0);
                if *tries >= TYPE_ATTEMPTS {
                    return String::new();
                }
                *tries += 1;
                let name = duplicate(process.raw(), e.value as u64).and_then(|d| {
                    let r = query_object_string(d, 2);
                    // SAFETY: d 是 duplicate 返回的、本函数独占的句柄。
                    let _ = unsafe { CloseHandle(d) };
                    r
                });
                match name {
                    Some(n) if !n.is_empty() => {
                        if let Ok(mut c) = type_cache().lock() {
                            c.insert(e.type_index, n.clone());
                        }
                        n
                    }
                    _ => String::new(),
                }
            });
            Handle { value: e.value as u64, type_index: e.type_index, type_name, access: e.access }
        })
        .collect();
    out.sort_by_key(|h| h.value);
    Ok(out)
}

/// 给句柄查名称的工具。只能在一个线程里用（内部带“卡住次数”状态）；可以整个移到工作线程。
pub struct HandleNamer {
    process: OwnedHandle,
    stuck: u32,
}

// SAFETY: 内部只有一个进程句柄（内核对象句柄，可跨线程使用）与计数。
unsafe impl Send for HandleNamer {}

impl HandleNamer {
    pub fn open(key: ProcessKey) -> Result<Self, HandleError> {
        if key.pid <= 4 {
            return Err(HandleError::Protected);
        }
        Ok(Self { process: open_checked(key, PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE)?, stuck: 0 })
    }

    /// 查一个句柄的名称，最多等 `timeout`。
    pub fn name(&mut self, h: &Handle, timeout: Duration) -> NameResult {
        // 已知会卡死的组合：同步文件对象（命名管道、串口等）带这些访问掩码时 NtQueryObject 不返回。
        const HANG_ACCESS: [u32; 4] = [0x0012_019F, 0x001A_019F, 0x0012_0189, 0x0010_0000];
        if h.type_name == "File" && (HANG_ACCESS.contains(&h.access) || self.stuck >= MAX_STUCK) {
            return NameResult::TimedOut;
        }
        let Some(dup) = duplicate(self.process.raw(), h.value) else {
            return NameResult::Failed;
        };
        let raw = dup.0 as usize;
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("handle-name".into()).spawn(move || {
            let dup = HANDLE(raw as *mut c_void);
            let r = query_object_string(dup, 1);
            // SAFETY: 句柄由本线程独占，用完关闭。
            let _ = unsafe { CloseHandle(dup) };
            let _ = tx.send(r);
        });
        if spawned.is_err() {
            // SAFETY: 线程没起来，句柄还在本线程手里。
            let _ = unsafe { CloseHandle(dup) };
            return NameResult::Failed;
        }
        match rx.recv_timeout(timeout) {
            Ok(Some(n)) if n.is_empty() => NameResult::Unnamed,
            Ok(Some(n)) => NameResult::Name(n),
            Ok(None) => NameResult::Failed,
            Err(_) => {
                self.stuck += 1;
                NameResult::TimedOut
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Threading::CreateEventW;

    fn me() -> ProcessKey {
        let pid = std::process::id();
        let h = crate::control::open(pid, PROCESS_QUERY_INFORMATION).unwrap();
        ProcessKey { pid, create_time: crate::control::create_time(&h).unwrap() }
    }

    #[test]
    fn own_handles_include_a_named_event_with_its_name() {
        let name = format!("tml-handles-test-{}", std::process::id());
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let ev = unsafe { CreateEventW(None, true, false, windows::core::PCWSTR(wide.as_ptr())) }.unwrap();
        let list = list_handles(me()).expect("读取自身句柄");
        let mine = list.iter().find(|h| h.value == ev.0 as usize as u64).expect("应有刚创建的事件");
        assert_eq!(mine.type_name, "Event");
        assert!(list.windows(2).all(|w| w[0].value < w[1].value));
        let mut namer = HandleNamer::open(me()).unwrap();
        match namer.name(mine, Duration::from_secs(2)) {
            NameResult::Name(n) => assert!(n.ends_with(&name), "{n}"),
            other => panic!("{other:?}"),
        }
        unsafe { CloseHandle(ev).ok() };
    }

    #[test]
    fn naming_every_own_handle_finishes_quickly() {
        let list = list_handles(me()).unwrap();
        assert!(list.len() > 5);
        let mut namer = HandleNamer::open(me()).unwrap();
        let start = std::time::Instant::now();
        for h in &list {
            let _ = namer.name(h, Duration::from_millis(200));
        }
        assert!(start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());
    }

    #[test]
    fn gone_when_create_time_does_not_match() {
        let mut k = me();
        k.create_time += 1;
        assert_eq!(list_handles(k), Err(HandleError::Gone));
    }

    #[test]
    fn system_processes_are_protected() {
        let k = ProcessKey { pid: 4, create_time: 0 };
        assert_eq!(list_handles(k), Err(HandleError::Protected));
    }
}

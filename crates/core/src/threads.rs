//! 进程线程列表（#90，进程详情窗口“线程”页签）。
//!
//! 数据来自 `NtQuerySystemInformation(SystemExtendedProcessInformation)` 的线程数组，不需要逐线程打开句柄；
//! 起始地址取 Win32 起始地址（线程函数），再用 #74 的模块列表解析成“模块名 + 偏移”。
//! 核心库不返回显示文案：状态用枚举，等待原因保留系统原始值，文案在界面层 `texts.rs`。

use crate::error::SystemError;
use crate::modules::Module;
use crate::nt::{self, RawThread};
use crate::snapshot::ProcessKey;

/// 线程状态（KTHREAD_STATE；等待且原因为挂起时单列为 `Suspended`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    Initialized,
    Ready,
    Running,
    Standby,
    Terminated,
    Waiting,
    Transition,
    DeferredReady,
    /// 因挂起而等待（与进程的“已挂起”同一判定）。
    Suspended,
    Other(u32),
}

impl ThreadState {
    pub fn from_raw(state: u32, wait_reason: u32) -> Self {
        if nt::thread_is_suspended(state, wait_reason) {
            return Self::Suspended;
        }
        match state {
            0 => Self::Initialized,
            1 => Self::Ready,
            2 => Self::Running,
            3 => Self::Standby,
            4 => Self::Terminated,
            5 => Self::Waiting,
            6 => Self::Transition,
            7 => Self::DeferredReady,
            n => Self::Other(n),
        }
    }
}

/// 一个线程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub tid: u32,
    pub priority: i32,
    pub base_priority: i32,
    pub state: ThreadState,
    /// KWAIT_REASON 原始值；只有 `Waiting` 时有意义。
    pub wait_reason: u32,
    /// 用户态 + 内核态 CPU 时间，100ns 单位。
    pub cpu_time: u64,
    pub context_switches: u32,
    pub start_address: u64,
}

impl From<RawThread> for Thread {
    fn from(t: RawThread) -> Self {
        Self {
            tid: t.tid,
            priority: t.priority,
            base_priority: t.base_priority,
            state: ThreadState::from_raw(t.state, t.wait_reason),
            wait_reason: t.wait_reason,
            cpu_time: t.cpu_time,
            context_switches: t.context_switches,
            start_address: t.start_address,
        }
    }
}

/// 读取线程列表失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadError {
    /// 进程已退出，或 PID 已被别的进程复用。
    Gone,
    System(SystemError),
}

impl ThreadError {
    /// 日志用错误码；与系统错误码无关的原因为 0。
    pub fn code(&self) -> i32 {
        match self {
            Self::System(SystemError::Win32(c)) => *c,
            Self::System(_) | Self::Gone => 0,
        }
    }
}

impl std::fmt::Display for ThreadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone => f.write_str("process is gone"),
            Self::System(e) => write!(f, "cannot list threads: {e}"),
        }
    }
}

impl std::error::Error for ThreadError {}

/// 列出进程的线程，按线程 ID 升序。
pub fn list_threads(key: ProcessKey) -> Result<Vec<Thread>, ThreadError> {
    match nt::threads_of(key.pid, key.create_time) {
        Ok(Some(raw)) => {
            let mut out: Vec<Thread> = raw.into_iter().map(Thread::from).collect();
            out.sort_by_key(|t| t.tid);
            Ok(out)
        }
        Ok(None) => Err(ThreadError::Gone),
        Err(e) => Err(ThreadError::System(SystemError::Win32(e.code().0))),
    }
}

/// 把地址解析成（模块名, 偏移）；落在任何模块之外返回 `None`。`modules` 需按基址升序（`list_modules` 的顺序）。
pub fn locate<'a>(addr: u64, modules: &'a [Module]) -> Option<(&'a str, u64)> {
    let i = modules.partition_point(|m| m.base <= addr).checked_sub(1)?;
    let m = &modules[i];
    (addr < m.base + m.size as u64).then(|| (m.name.as_str(), addr - m.base))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn me() -> ProcessKey {
        let pid = std::process::id();
        let h = crate::control::open(pid, windows::Win32::System::Threading::PROCESS_QUERY_INFORMATION).unwrap();
        ProcessKey { pid, create_time: crate::control::create_time(&h).unwrap() }
    }

    fn module(name: &str, base: u64, size: u32) -> Module {
        Module { name: name.into(), path: PathBuf::from(name), base, size }
    }

    #[test]
    fn own_threads_include_the_current_one_and_resolve_start_addresses() {
        let threads = list_threads(me()).expect("读取自身线程");
        assert!(!threads.is_empty());
        assert!(threads.windows(2).all(|w| w[0].tid < w[1].tid));
        let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
        let mine = threads.iter().find(|t| t.tid == tid).expect("应有当前线程");
        assert!(matches!(mine.state, ThreadState::Running | ThreadState::Ready | ThreadState::Standby), "{mine:?}");
        // 线程数与进程快照一致（允许两次读取之间测试线程自己增减）。
        let procs = nt::ProcessQuery::default().query().unwrap();
        let p = procs.iter().find(|p| p.pid == std::process::id()).unwrap();
        assert!((p.threads as i64 - threads.len() as i64).abs() <= 4);
        // 起始地址能落进某个模块（测试线程的起点通常在 ntdll / kernel32 / 本程序里）。
        let mods = crate::modules::list_modules(me()).unwrap();
        assert!(threads.iter().any(|t| locate(t.start_address, &mods).is_some()));
    }

    #[test]
    fn gone_when_create_time_does_not_match() {
        let mut k = me();
        k.create_time += 1;
        assert_eq!(list_threads(k), Err(ThreadError::Gone));
    }

    #[test]
    fn locate_finds_module_and_offset() {
        let mods = [module("a.dll", 0x1000, 0x100), module("b.dll", 0x2000, 0x500)];
        assert_eq!(locate(0x1010, &mods), Some(("a.dll", 0x10)));
        assert_eq!(locate(0x24FF, &mods), Some(("b.dll", 0x4FF)));
        assert_eq!(locate(0x1100, &mods), None);
        assert_eq!(locate(0x500, &mods), None);
        assert_eq!(locate(0x2500, &mods), None);
    }

    #[test]
    fn suspended_uses_the_shared_rule() {
        assert_eq!(ThreadState::from_raw(5, 5), ThreadState::Suspended);
        assert_eq!(ThreadState::from_raw(5, 6), ThreadState::Waiting);
        assert_eq!(ThreadState::from_raw(2, 0), ThreadState::Running);
    }
}

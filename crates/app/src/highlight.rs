//! 进程列表的行标记（#79 #80）。
//!
//! - 变化高亮：新出现的进程行浅绿、刚退出的行保留为浅红（“幽灵行”）约 `duration`，之后淡出消失。
//!   幽灵行是退出前最后一次快照里的进程副本，追加在新快照末尾，让排序、树形与选中沿用现有逻辑。
//! - 类型标记：关键 / 已提权 / 系统账户 / 承载服务的进程在行首画一条细色条，与变化高亮的背景互不冲突。
//!
//! 首份快照不高亮；窗口隐藏期间累积的变化在下一份快照里合并成一次；过期不依赖动画（减弱动态效果下同样适用）。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use tml_core::{Process, ProcessKey};

/// 行的变化状态（对应 `ProcRow.change`）。
pub const CHANGE_NONE: i32 = 0;
pub const CHANGE_NEW: i32 = 1;
pub const CHANGE_EXITED: i32 = 2;

/// 行的类型标记（对应 `ProcRow.kind`），数值越大优先级越高。
pub const KIND_NONE: i32 = 0;
pub const KIND_SERVICE: i32 = 1;
pub const KIND_SYSTEM: i32 = 2;
pub const KIND_ELEVATED: i32 = 3;
pub const KIND_CRITICAL: i32 = 4;

#[derive(Default)]
pub struct ChangeTracker {
    born: HashMap<ProcessKey, Instant>,
    ghosts: HashMap<ProcessKey, Instant>,
}

impl ChangeTracker {
    /// 用新快照更新状态。`old` 是当前显示的快照（可能含上次追加的幽灵行），`new` 是刚采到的快照，
    /// 返回前会把仍在保留期内的幽灵行追加到 `new` 末尾。返回本次新增与退出的进程数（日志用）。`enabled` 为假时清空一切，行为与没有这个功能相同。
    pub fn apply(&mut self, old: &[Process], new: &mut Vec<Process>, now: Instant, duration: Duration, enabled: bool) -> (usize, usize) {
        let mut events = (0, 0);
        if !enabled {
            self.born.clear();
            self.ghosts.clear();
            return events;
        }
        let new_keys: HashSet<ProcessKey> = new.iter().map(|p| p.key).collect();
        if !old.is_empty() {
            let mut old_real: HashSet<ProcessKey> = HashSet::with_capacity(old.len());
            for p in old {
                if self.ghosts.contains_key(&p.key) {
                    continue;
                }
                old_real.insert(p.key);
                if !new_keys.contains(&p.key) {
                    self.born.remove(&p.key);
                    self.ghosts.insert(p.key, now);
                    events.1 += 1;
                }
            }
            for p in new.iter() {
                if !old_real.contains(&p.key) {
                    self.born.insert(p.key, now);
                    events.0 += 1;
                }
            }
        }
        self.born.retain(|k, t| new_keys.contains(k) && now.duration_since(*t) < duration);
        self.ghosts.retain(|_, t| now.duration_since(*t) < duration);
        for p in old {
            if self.ghosts.contains_key(&p.key) && !new_keys.contains(&p.key) {
                let mut ghost = p.clone();
                ghost.cpu_usage = 0.0;
                ghost.disk_rate = 0.0;
                ghost.gpu_usage = None;
                ghost.net_rate = None;
                new.push(ghost);
            }
        }
        events
    }

    pub fn change_of(&self, key: ProcessKey) -> i32 {
        if self.ghosts.contains_key(&key) {
            CHANGE_EXITED
        } else if self.born.contains_key(&key) {
            CHANGE_NEW
        } else {
            CHANGE_NONE
        }
    }

    /// 当前保留的幽灵行数（不计入进程总数）。
    pub fn ghost_count(&self) -> usize {
        self.ghosts.len()
    }

    /// 还有处于保留期的行，需要在下次采样时重算。
    pub fn is_active(&self) -> bool {
        !self.born.is_empty() || !self.ghosts.is_empty()
    }
}

/// 进程的类型标记；`user` 是已读到的账户名（没读到则不判系统账户）。
pub fn kind_of(critical: bool, elevated: Option<bool>, user: Option<&str>, hosts_services: bool) -> i32 {
    if critical {
        KIND_CRITICAL
    } else if elevated == Some(true) {
        KIND_ELEVATED
    } else if user.is_some_and(is_system_account) {
        KIND_SYSTEM
    } else if hosts_services {
        KIND_SERVICE
    } else {
        KIND_NONE
    }
}

/// SYSTEM、LOCAL SERVICE、NETWORK SERVICE（账户名可能带 `NT AUTHORITY\` 前缀，也可能是本地化名称，这里只认英文）。
fn is_system_account(user: &str) -> bool {
    let name = user.rsplit('\\').next().unwrap_or(user);
    ["SYSTEM", "LOCAL SERVICE", "NETWORK SERVICE"].iter().any(|n| name.eq_ignore_ascii_case(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tml_core::{Counters, ProcessKey};

    fn proc(pid: u32) -> Process {
        Process {
            key: ProcessKey { pid, create_time: 1 },
            parent_pid: 0,
            name: format!("p{pid}"),
            cpu_usage: 5.0,
            private_working_set: 0,
            disk_rate: 3.0,
            gpu_usage: None,
            net_rate: None,
            eco_mode: None,
            services: None,
            critical: false,
            counters: Counters::default(),
            info: None,
        }
    }

    fn key(pid: u32) -> ProcessKey {
        ProcessKey { pid, create_time: 1 }
    }

    const DUR: Duration = Duration::from_millis(1500);

    #[test]
    fn first_snapshot_has_no_highlight() {
        let mut t = ChangeTracker::default();
        let mut new = vec![proc(1), proc(2)];
        t.apply(&[], &mut new, Instant::now(), DUR, true);
        assert_eq!(new.len(), 2);
        assert!(!t.is_active());
    }

    #[test]
    fn new_process_is_green_then_expires() {
        let mut t = ChangeTracker::default();
        let t0 = Instant::now();
        let old = vec![proc(1)];
        let mut new = vec![proc(1), proc(2)];
        t.apply(&old, &mut new, t0, DUR, true);
        assert_eq!(t.change_of(key(2)), CHANGE_NEW);
        assert_eq!(t.change_of(key(1)), CHANGE_NONE);
        let mut later = new.clone();
        t.apply(&new, &mut later, t0 + Duration::from_secs(2), DUR, true);
        assert_eq!(t.change_of(key(2)), CHANGE_NONE);
    }

    #[test]
    fn exited_process_stays_as_ghost_then_disappears() {
        let mut t = ChangeTracker::default();
        let t0 = Instant::now();
        let old = vec![proc(1), proc(2)];
        let mut new = vec![proc(1)];
        t.apply(&old, &mut new, t0, DUR, true);
        assert_eq!(new.len(), 2);
        assert_eq!(new[1].key, key(2));
        assert_eq!(new[1].cpu_usage, 0.0);
        assert_eq!(t.change_of(key(2)), CHANGE_EXITED);
        assert_eq!(t.ghost_count(), 1);

        // 保留期内下一份快照：幽灵行仍在，且不会被当成新进程。
        let shown = new.clone();
        let mut next = vec![proc(1)];
        t.apply(&shown, &mut next, t0 + Duration::from_secs(1), DUR, true);
        assert_eq!(next.len(), 2);
        assert_eq!(t.change_of(key(2)), CHANGE_EXITED);

        // 过期：幽灵行移除。
        let shown = next.clone();
        let mut last = vec![proc(1)];
        t.apply(&shown, &mut last, t0 + Duration::from_secs(2), DUR, true);
        assert_eq!(last.len(), 1);
        assert!(!t.is_active());
    }

    #[test]
    fn disabled_clears_everything() {
        let mut t = ChangeTracker::default();
        let t0 = Instant::now();
        let mut new = vec![proc(1), proc(2)];
        t.apply(&[proc(1)], &mut new, t0, DUR, true);
        assert!(t.is_active());
        let mut again = vec![proc(1)];
        t.apply(&new, &mut again, t0, DUR, false);
        assert_eq!(again.len(), 1);
        assert!(!t.is_active());
    }

    #[test]
    fn kind_priority() {
        assert_eq!(kind_of(true, Some(true), Some("SYSTEM"), true), KIND_CRITICAL);
        assert_eq!(kind_of(false, Some(true), Some("SYSTEM"), true), KIND_ELEVATED);
        assert_eq!(kind_of(false, Some(false), Some(r"NT AUTHORITY\SYSTEM"), true), KIND_SYSTEM);
        assert_eq!(kind_of(false, None, Some("alice"), true), KIND_SERVICE);
        assert_eq!(kind_of(false, None, None, false), KIND_NONE);
    }
}

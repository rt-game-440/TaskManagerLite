//! 创建转储文件（issue #75）：进程页右键菜单“创建转储文件”→“最小…”“完整…”。
//!
//! 保存对话框与 `MiniDumpWriteDump` 都在后台线程里完成，界面线程只显示进度与结果。完整转储先弹确认框，
//! 显示估算体积；写完后弹出提示，可一键打开所在文件夹。日志只记 PID、类型、结果与错误码，不记路径。

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use slint::ComponentHandle;
use tml_core::{DumpError, DumpKind, ProcessKey};

use super::process_menu::failure;
use super::{CONFIRM_DUMP, CONFIRM_DUMP_DONE, Controller, STATUS_ERROR};
use crate::texts;
use crate::{AppWindow, ProcessMenu, file_dialog};

/// 状态栏：正在创建转储（参数为进程名）。
const STATUS_DUMPING: i32 = 15;

/// 转储的运行状态。
#[derive(Default)]
pub(super) struct DumpState {
    /// 保存对话框已打开或正在写转储，不再接受新的转储。
    busy: Cell<bool>,
    /// 等待完整转储确认的目标。
    pending: Cell<Option<ProcessKey>>,
    /// 最近一次成功写出的转储文件（提示框里“打开所在文件夹”用）。
    last: RefCell<Option<PathBuf>>,
}

/// 默认文件名：`<进程名去扩展名>.<PID>.dmp`。
pub fn default_file_name(process: &str, pid: u32) -> String {
    let stem = process.rsplit_once('.').map_or(process, |(s, _)| s);
    format!("{stem}.{pid}.dmp")
}

/// 估算体积的显示值（MB，向上取整，至少 1）；读不到估算时为 -1。
pub fn size_mb(bytes: Option<u64>) -> i32 {
    bytes.map_or(-1, |b| b.div_ceil(1024 * 1024).clamp(1, i32::MAX as u64) as i32)
}

enum Outcome {
    Cancelled,
    Saved(PathBuf),
    Failed(DumpError),
}

impl Controller {
    pub(super) fn bind_dump(self: &Rc<Self>, ui: &AppWindow) {
        let menu = ui.global::<ProcessMenu>();
        let c = Rc::downgrade(self);
        menu.on_create_dump(move |full| {
            if let Some(c) = c.upgrade() {
                c.request_dump(if full { DumpKind::Full } else { DumpKind::Mini });
            }
        });
    }

    /// 菜单弹出前：能否转储，不能时标题里写原因。
    pub(super) fn update_dump_menu(&self, ui: &AppWindow) {
        let block = match self.selected_process() {
            Some((key, ..)) => tml_core::can_dump_process(key).err(),
            None => Some(DumpError::Gone),
        };
        let menu = ui.global::<ProcessMenu>();
        menu.set_can_dump(block.is_none());
        menu.set_dump_title(texts::dump_menu_title(block.as_ref()).into());
    }

    fn request_dump(&self, kind: DumpKind) {
        let Some((key, name, _)) = self.selected_process() else { return };
        if self.dump.busy.get() {
            self.report(STATUS_ERROR, failure(texts::dump_action(), "上一个转储还没有完成"));
            return;
        }
        if let Err(e) = tml_core::can_dump_process(key) {
            self.log.borrow_mut().line(format!("转储 拒绝 pid={} 错误码={}", key.pid, e.code()));
            self.report(STATUS_ERROR, failure(texts::dump_action(), &texts::dump_error(&e)));
            return;
        }
        if kind == DumpKind::Full {
            let mb = size_mb(tml_core::dump_size_estimate(key));
            self.log.borrow_mut().line(format!("转储 完整 pid={} 等待确认 估算MB={mb}", key.pid));
            self.dump.pending.set(Some(key));
            self.ask(CONFIRM_DUMP, Vec::new(), name, mb, false);
        } else {
            self.start_dump(key, name, kind);
        }
    }

    /// 确认框点“继续”（完整转储）。
    pub(super) fn confirm_dump(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let name = ui.get_confirm_name().to_string();
        if let Some(key) = self.dump.pending.take() {
            self.start_dump(key, name, DumpKind::Full);
        }
    }

    /// 完成提示框点“打开所在文件夹”。
    pub(super) fn confirm_dump_done(&self) {
        let Some(path) = self.dump.last.borrow().clone() else { return };
        if let Err(e) = tml_core::reveal_in_explorer(&path) {
            self.log.borrow_mut().line(format!("转储 打开所在文件夹失败 {e:?}"));
            self.report(STATUS_ERROR, failure("打开所在文件夹", &texts::shell_error(&e)));
        }
    }

    fn start_dump(&self, key: ProcessKey, name: String, kind: DumpKind) {
        if self.dump.busy.replace(true) {
            return;
        }
        let owner = self.with_active(super::mini::hwnd_of).flatten().map_or(0, |h| h.0 as isize);
        let file = default_file_name(&name, key.pid);
        // 自动化验收：环境变量指定目标路径时不弹保存对话框。
        let preset = std::env::var_os("TML_DUMP_PATH").map(PathBuf::from);
        self.log.borrow_mut().line(format!("转储 {} 开始 pid={}", texts::dump_kind_name(kind), key.pid));
        self.report(STATUS_DUMPING, name.clone());
        let spawned = std::thread::Builder::new().name("create-dump".into()).spawn(move || {
            let path = preset.or_else(|| {
                let default = std::env::temp_dir().join(&file);
                file_dialog::pick_save_file(
                    owner,
                    texts::dump_save_title(),
                    &default.to_string_lossy(),
                    "dmp",
                    &texts::dump_filters(),
                )
            });
            let outcome = match path {
                None => Outcome::Cancelled,
                Some(path) => match tml_core::write_dump(key, &path, kind) {
                    Ok(()) => Outcome::Saved(path),
                    Err(e) => Outcome::Failed(e),
                },
            };
            let _ = slint::invoke_from_event_loop(move || {
                crate::CONTROLLER.with(|c| {
                    if let Some(c) = c.borrow().as_ref() {
                        c.finish_dump(key, name, kind, outcome);
                    }
                })
            });
        });
        if spawned.is_err() {
            self.dump.busy.set(false);
            self.report(STATUS_ERROR, failure(texts::dump_action(), "无法启动后台线程"));
        }
    }

    fn finish_dump(&self, key: ProcessKey, name: String, kind: DumpKind, outcome: Outcome) {
        self.dump.busy.set(false);
        let what = texts::dump_kind_name(kind);
        match outcome {
            Outcome::Cancelled => {
                self.log.borrow_mut().line(format!("转储 {what} 已取消 pid={}", key.pid));
                self.report(0, String::new());
            }
            Outcome::Saved(path) => {
                self.log.borrow_mut().line(format!("转储 {what} 成功 pid={}", key.pid));
                let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                *self.dump.last.borrow_mut() = Some(path);
                self.report(0, String::new());
                if std::env::var_os("TML_DUMP_PATH").is_none() {
                    self.ask(CONFIRM_DUMP_DONE, Vec::new(), file, 0, false);
                }
            }
            Outcome::Failed(e) => {
                self.log.borrow_mut().line(format!("转储 {what} 失败 pid={} 错误码={}", key.pid, e.code()));
                self.report(STATUS_ERROR, failure(texts::dump_action(), &format!("{name}：{}", texts::dump_error(&e))));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_name_is_stem_dot_pid() {
        assert_eq!(default_file_name("notepad.exe", 1234), "notepad.1234.dmp");
        assert_eq!(default_file_name("System", 4), "System.4.dmp");
        assert_eq!(default_file_name("a.b.exe", 7), "a.b.7.dmp");
    }

    #[test]
    fn size_is_rounded_up_to_mb() {
        assert_eq!(size_mb(None), -1);
        assert_eq!(size_mb(Some(0)), 1);
        assert_eq!(size_mb(Some(1)), 1);
        assert_eq!(size_mb(Some(1024 * 1024)), 1);
        assert_eq!(size_mb(Some(1024 * 1024 + 1)), 2);
    }
}

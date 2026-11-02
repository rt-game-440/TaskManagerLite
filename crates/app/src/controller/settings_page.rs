//! 设置页（#16）：`SettingsData` 的初始值、回调，以及开机启动与接管任务管理器的状态读取。
//!
//! 开机启动与接管任务管理器的开关状态以注册表为准，每次进入设置页时重新读取；其余开关以设置文件为准，改动后立即保存。

use crate::i18n::{tr, trf};
use std::path::Path;
use std::rc::Rc;

use slint::ComponentHandle;
use tml_core::{Autostart, AutostartError, AutostartKey, IfeoError, Registration};

use super::Controller;
use crate::sampling::Command;
use crate::texts;
use crate::{AppWindow, SettingsData};

/// 开机启动的状态说明。
fn autostart_note(status: &Result<Autostart, AutostartError>) -> String {
    match status {
        Ok(Autostart::Off | Autostart::On) => String::new(),
        Ok(Autostart::OtherPath(value)) => {
            trf("已登记的是另一个位置的程序（{0}），开启后改为当前程序", &[&value])
        }
        Ok(Autostart::DisabledBySystem) => tr("已在 Windows 设置的“启动应用”中被禁用，重新开启即可恢复").into(),
        Err(e) => trf("无法读取开机启动状态：{0}", &[&(texts::autostart_error(e))]),
    }
}

/// 接管任务管理器的状态说明与是否为错误。`wanted` 是设置里记录的意愿。
fn taskmgr_note(status: &Result<Registration, IfeoError>, wanted: bool) -> (String, bool) {
    match status {
        Ok(Registration::ThisExe) => (String::new(), false),
        Ok(Registration::NotRegistered) if wanted => (tr("登记已被移除，重新开启即可恢复").into(), false),
        Ok(Registration::NotRegistered) => (String::new(), false),
        Ok(Registration::OtherExe { debugger, exists: true }) => {
            (trf("已被其他程序接管：{0}", &[&(texts::debugger_value(debugger))]), false)
        }
        Ok(Registration::OtherExe { debugger, exists: false }) => {
            (trf("登记的程序已不存在（{0}），开启后改为本程序", &[&(texts::debugger_value(debugger))]), false)
        }
        Err(e) => (trf("无法读取登记状态：{0}", &[&(texts::ifeo_error(e))]), true),
    }
}

/// 检查更新的状态（#99）。
#[derive(Default)]
pub(super) struct UpdateState {
    busy: std::cell::Cell<bool>,
    /// “打开 Releases 页面”要打开的地址：发现新版本时为该版本发布页。
    page: std::cell::RefCell<Option<String>>,
}

fn exe() -> std::io::Result<std::path::PathBuf> {
    std::env::current_exe()
}

impl Controller {
    pub(super) fn bind_settings(self: &Rc<Self>, ui: &AppWindow) {
        let data = ui.global::<SettingsData>();
        {
            let s = self.settings.borrow();
            data.set_close_to_tray(s.close_to_tray);
            data.set_theme(s.theme);
            data.set_language(s.language);
            data.set_background_sampling(s.background_sampling);
            data.set_highlight_changes(s.highlight_changes);
            data.set_row_marks(s.row_marks);
            data.set_accurate_disk(s.accurate_disk_io);
            data.set_replace_taskmgr(s.replace_taskmgr);
            // 0（跟随系统）是界面的默认状态，无需设置。
            if s.theme != 0 {
                ui.invoke_set_theme(s.theme);
            }
        }
        data.set_settings_path(self.store.as_ref().map(|s| s.path().display().to_string()).unwrap_or_default().into());

        let c = Rc::downgrade(self);
        data.on_refresh(move || {
            if let Some(c) = c.upgrade() {
                c.refresh_settings_page();
            }
        });
        let c = Rc::downgrade(self);
        data.on_autostart_toggled(move |on| {
            if let Some(c) = c.upgrade() {
                c.set_autostart(on);
            }
        });
        let c = Rc::downgrade(self);
        data.on_close_to_tray_toggled(move |on| {
            if let Some(c) = c.upgrade() {
                c.set_close_to_tray(on);
                c.save_settings();
            }
        });
        let c = Rc::downgrade(self);
        data.on_highlight_changes_toggled(move |on| {
            if let Some(c) = c.upgrade() {
                c.settings.borrow_mut().highlight_changes = on;
                c.refresh_rows();
                c.save_settings();
            }
        });
        let c = Rc::downgrade(self);
        data.on_row_marks_toggled(move |on| {
            if let Some(c) = c.upgrade() {
                c.settings.borrow_mut().row_marks = on;
                let services = on || c.state.borrow().details.iter().any(|d| d.column == tml_core::DetailColumn::Services);
                c.sampler.send(Command::Services(services));
                c.refresh_rows();
                c.save_settings();
            }
        });
        let c = Rc::downgrade(self);
        data.on_language_selected(move |option| {
            if let Some(c) = c.upgrade() {
                c.set_language(option);
            }
        });
        let c = Rc::downgrade(self);
        data.on_theme_selected(move |mode| {
            if let Some(c) = c.upgrade() {
                c.set_theme(mode);
            }
        });
        let c = Rc::downgrade(self);
        data.on_background_sampling_toggled(move |on| {
            if let Some(c) = c.upgrade() {
                c.set_background_sampling(on);
                c.save_settings();
            }
        });
        let c = Rc::downgrade(self);
        data.on_accurate_disk_toggled(move |on| {
            if let Some(c) = c.upgrade() {
                c.settings.borrow_mut().accurate_disk_io = on;
                c.sampler.send(Command::AccurateDisk(on));
                c.log.borrow_mut().line(format!("精确磁盘统计 意愿={on}"));
                c.save_settings();
            }
        });
        let c = Rc::downgrade(self);
        data.on_replace_taskmgr_toggled(move |on| {
            if let Some(c) = c.upgrade() {
                c.set_replace_taskmgr(on);
            }
        });
        let c = Rc::downgrade(self);
        data.on_open_settings_folder(move || {
            if let Some(c) = c.upgrade() {
                c.open_settings_folder();
            }
        });
        data.set_app_version(env!("CARGO_PKG_VERSION").into());
        let c = Rc::downgrade(self);
        data.on_check_update(move || {
            if let Some(c) = c.upgrade() {
                c.check_update(false);
            }
        });
        let c = Rc::downgrade(self);
        data.on_open_releases(move || {
            if let Some(c) = c.upgrade() {
                c.open_releases();
            }
        });
    }

    /// 检查更新（#99）：联网放到后台线程，结果回到界面线程显示。`exit_after` 供 `--check-update` 钩子用。
    pub fn check_update(&self, exit_after: bool) {
        if self.update.busy.replace(true) {
            return;
        }
        self.settings_data(|d| d.set_update_busy(true));
        let url = tml_core::update::update_url();
        let custom = url != tml_core::update::LATEST_RELEASE_API;
        self.log.borrow_mut().line(format!("检查更新 开始 自定义地址={custom}"));
        let started = std::time::Instant::now();
        let spawned = std::thread::Builder::new().name("check-update".into()).spawn(move || {
            let result = tml_core::update::check_for_update(env!("CARGO_PKG_VERSION"), &url);
            let elapsed = started.elapsed();
            let _ = slint::invoke_from_event_loop(move || {
                let controller = crate::CONTROLLER.with(|c| c.borrow().clone());
                if let Some(c) = controller {
                    c.finish_check_update(result, elapsed, exit_after);
                }
            });
        });
        if let Err(e) = spawned {
            let error = tml_core::update::UpdateError::Network(e.into());
            self.finish_check_update(Err(error), started.elapsed(), exit_after);
        }
    }

    fn finish_check_update(
        &self,
        result: Result<tml_core::update::UpdateStatus, tml_core::update::UpdateError>,
        elapsed: std::time::Duration,
        exit_after: bool,
    ) {
        use tml_core::update::{RELEASES_PAGE, UpdateStatus, notes_summary};
        self.update.busy.set(false);
        // 日志只记结果、版本号与错误码，不记发布说明正文。
        let line = match &result {
            Ok(UpdateStatus::UpToDate { latest }) => format!("检查更新 结果=最新 最新版={latest}"),
            Ok(UpdateStatus::Available(r)) => format!("检查更新 结果=有新版本 最新版={} 发布页={}", r.version, r.url),
            Err(e) => format!("检查更新 结果=失败 原因={e:?} 错误码={:?}", e.code()),
        };
        self.log.borrow_mut().line(format!("{line} 耗时={} ms", elapsed.as_millis()));
        let (note, error, page) = match &result {
            Ok(status @ UpdateStatus::Available(r)) => {
                let summary = notes_summary(&r.notes, 4, 240);
                (texts::update_status(status, &summary), false, r.url.clone())
            }
            Ok(status) => (texts::update_status(status, ""), false, RELEASES_PAGE.to_string()),
            Err(e) => (texts::update_error(e), true, RELEASES_PAGE.to_string()),
        };
        *self.update.page.borrow_mut() = Some(page);
        self.settings_data(|d| {
            d.set_update_busy(false);
            d.set_update_note(note.into());
            d.set_update_error(error);
        });
        if exit_after {
            let _ = slint::quit_event_loop();
        }
    }

    /// 打开最近一次发现的新版本发布页，没有则打开 Releases 列表。
    fn open_releases(&self) {
        let page = self.update.page.borrow().clone().unwrap_or_else(|| tml_core::update::RELEASES_PAGE.to_string());
        let result = tml_core::open_url(&page);
        self.log.borrow_mut().line(format!("打开发布页 结果={result:?}"));
        if let Err(e) = result {
            self.settings_data(|d| {
                d.set_update_note(texts::shell_error(&e).into());
                d.set_update_error(true);
            });
        }
    }

    /// 进入设置页时重新读取以注册表为准的两项（用户可能在系统设置或别的工具里改过）。
    pub fn refresh_settings_page(&self) {
        self.refresh_autostart(None);
        if !self.taskmgr_busy() {
            self.refresh_taskmgr(None);
        }
    }

    fn settings_data(&self, f: impl FnOnce(SettingsData<'_>)) {
        if let Some(ui) = self.ui.upgrade() {
            f(ui.global::<SettingsData>());
        }
    }

    /// 读取开机启动状态写回界面；`error` 是刚才修改失败的原因，优先显示。
    fn refresh_autostart(&self, error: Option<String>) {
        let status = AutostartKey::from_env().and_then(|key| key.status(&exe()?));
        let note = error.unwrap_or_else(|| autostart_note(&status));
        let on = status.as_ref().is_ok_and(Autostart::is_on);
        self.settings_data(|d| {
            d.set_autostart(on);
            d.set_autostart_note(note.into());
        });
    }

    fn set_autostart(&self, on: bool) {
        let result = AutostartKey::from_env().and_then(|key| key.set(&exe()?, on));
        self.log.borrow_mut().line(format!("开机启动 设为={on} 结果={result:?}"));
        self.refresh_autostart(result.err().map(|e| trf("无法修改开机启动：{0}", &[&(texts::autostart_error(&e))])));
    }

    /// 切换界面语言：Slint 文案由内置翻译立即刷新，Rust 文案（状态栏、托盘等）下一次刷新时按新语言重算。
    fn set_language(&self, option: i32) {
        let option = option.clamp(0, 2);
        self.settings.borrow_mut().language = option;
        crate::i18n::set_lang(crate::i18n::resolve(option));
        self.save_settings();
        self.refresh_after_language();
    }

    fn set_theme(&self, mode: i32) {
        let mode = mode.clamp(0, 2);
        self.settings.borrow_mut().theme = mode;
        if let Some(ui) = self.ui.upgrade() {
            ui.invoke_set_theme(mode);
        }
        if let Some(mini) = self.mini.get() {
            mini.invoke_set_theme(mode);
        }
        self.save_settings();
    }

    fn taskmgr_busy(&self) -> bool {
        self.ui.upgrade().is_some_and(|ui| ui.global::<SettingsData>().get_taskmgr_busy())
    }

    /// 读取 IFEO 登记状态写回界面；`error` 是刚才修改失败的原因，优先显示。
    fn refresh_taskmgr(&self, error: Option<IfeoError>) {
        let status = tml_core::Ifeo::from_env()
            .and_then(|ifeo| exe().map_err(|e| IfeoError::System(e.into())).and_then(|exe| ifeo.status(&exe)));
        let wanted = self.settings.borrow().replace_taskmgr;
        let (note, is_error) = match error {
            // 核心库的说明提到命令行的 --force，界面上换成不含它的说法。
            Some(IfeoError::UnsafeLocation(why)) => (texts::unsafe_location_note(&why), true),
            Some(e) => (texts::ifeo_error(&e), !matches!(e, IfeoError::Cancelled)),
            None => taskmgr_note(&status, wanted),
        };
        // 读不出状态时开关沿用设置里的意愿，只显示错误，免得已登记的用户误以为没有登记。
        let on = match status {
            Ok(r) => matches!(r, Registration::ThisExe),
            Err(_) => wanted,
        };
        self.settings_data(|d| {
            d.set_replace_taskmgr(on);
            d.set_taskmgr_note(note.into());
            d.set_taskmgr_error(is_error);
        });
    }

    /// 登记或撤销可能要弹出 UAC 并等待提权子进程，放到后台线程；结束后以注册表的实际状态为准。
    fn set_replace_taskmgr(&self, on: bool) {
        if self.taskmgr_busy() {
            return;
        }
        self.settings_data(|d| d.set_taskmgr_busy(true));
        self.log.borrow_mut().line(format!("接管任务管理器 设为={on}"));
        let spawned = std::thread::Builder::new().name("taskmgr-ifeo".into()).spawn(move || {
            let result = tml_core::set_replace_taskmgr(on, false);
            let _ = slint::invoke_from_event_loop(move || {
                let controller = crate::CONTROLLER.with(|c| c.borrow().clone());
                if let Some(c) = controller {
                    c.finish_replace_taskmgr(on, result);
                }
            });
        });
        if let Err(e) = spawned {
            self.finish_replace_taskmgr(on, Err(IfeoError::ThreadSpawnFailed(e.into())));
        }
    }

    fn finish_replace_taskmgr(&self, on: bool, result: Result<(), IfeoError>) {
        self.log.borrow_mut().line(format!("接管任务管理器 设为={on} 结果={result:?}"));
        self.settings_data(|d| d.set_taskmgr_busy(false));
        // 成功时记下新的意愿；失败（含取消）时保持原意愿，界面开关回到注册表的实际状态。
        if result.is_ok() {
            self.settings.borrow_mut().replace_taskmgr = on;
            self.save_settings();
        }
        self.refresh_taskmgr(result.err());
    }

    fn open_settings_folder(&self) {
        let Some(store) = &self.store else { return };
        // 先保存一次，确保文件存在、资源管理器能选中它。
        self.save_settings();
        let path: &Path = store.path();
        let result =
            tml_core::reveal_in_explorer(path).or_else(|_| path.parent().map_or(Ok(()), tml_core::reveal_in_explorer));
        if let Err(e) = result {
            self.log.borrow_mut().line(format!("打开设置文件夹失败 {e:?}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 开机启动说明() {
        assert_eq!(autostart_note(&Ok(Autostart::On)), "");
        assert_eq!(autostart_note(&Ok(Autostart::Off)), "");
        assert_eq!(
            autostart_note(&Ok(Autostart::DisabledBySystem)),
            "已在 Windows 设置的“启动应用”中被禁用，重新开启即可恢复"
        );
        assert_eq!(
            autostart_note(&Ok(Autostart::OtherPath(r"D:\old.exe".into()))),
            r"已登记的是另一个位置的程序（D:\old.exe），开启后改为当前程序"
        );
        let err = AutostartError::System(tml_core::SystemError::Win32(5));
        assert_eq!(
            autostart_note(&Err(err)),
            format!("无法读取开机启动状态：{}", std::io::Error::from_raw_os_error(5))
        );
        assert_eq!(
            autostart_note(&Err(AutostartError::InvalidTestKey)),
            r"无法读取开机启动状态：TML_AUTOSTART_TEST_KEY 必须以 Software\TaskManagerLite-test\ 开头且其后不为空"
        );
    }

    #[test]
    fn 接管说明() {
        assert_eq!(taskmgr_note(&Ok(Registration::ThisExe), true), (String::new(), false));
        assert_eq!(taskmgr_note(&Ok(Registration::NotRegistered), false), (String::new(), false));
        assert_eq!(
            taskmgr_note(&Ok(Registration::NotRegistered), true),
            ("登记已被移除，重新开启即可恢复".to_owned(), false)
        );
        let other = Registration::OtherExe { debugger: "x.exe".into(), exists: true };
        assert_eq!(taskmgr_note(&Ok(other), false), ("已被其他程序接管：x.exe".to_owned(), false));
        let gone = Registration::OtherExe { debugger: "x.exe".into(), exists: false };
        assert_eq!(taskmgr_note(&Ok(gone), true), ("登记的程序已不存在（x.exe），开启后改为本程序".to_owned(), false));
        // 非字符串值：描述与改动前相同。
        let dword = Registration::OtherExe { debugger: tml_core::DebuggerValue::NonString { value_type: 4 }, exists: true };
        assert_eq!(taskmgr_note(&Ok(dword), false), ("已被其他程序接管：<非字符串值，类型 4>".to_owned(), false));
        assert_eq!(
            taskmgr_note(&Err(IfeoError::AccessDenied), false),
            ("无法读取登记状态：没有写入系统注册表的权限（需要管理员）".to_owned(), true)
        );
    }
}

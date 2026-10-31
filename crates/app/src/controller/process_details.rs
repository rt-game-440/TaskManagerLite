//! 进程详情窗口（issue #74）：进程页右键菜单“详细信息…”打开独立窗口，先提供“模块”页签。
//!
//! 窗口是单例，再次打开会换成新的目标进程。只有窗口打开期间才有 1 秒一次的刷新定时器，关闭即停；
//! 模块的描述 / 公司要读文件版本资源，放在一次性后台线程里，结果在下一个定时器节拍取回，界面线程不阻塞。
//! 日志只记 PID、模块数、结果与错误码。

use crate::i18n::{tr, trf};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tml_core::ProcessKey;
use tml_core::modules::{self, Module};

use crate::settings::DetailsWindow;
use crate::texts;
use crate::{AppWindow, HandleRow, ModuleRow, ProcessDetailsWindow, ProcessMenu, ThreadRow};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tml_core::handles::{self, Handle, HandleNamer, NameResult};

/// 详情窗口的运行状态。
#[derive(Default)]
pub(super) struct DetailsState {
    window: RefCell<Option<ProcessDetailsWindow>>,
    target: Cell<Option<ProcessKey>>,
    name: RefCell<String>,
    timer: slint::Timer,
    /// 路径 → (描述, 公司)，同一个 DLL 只读一次。
    versions: RefCell<HashMap<PathBuf, (String, String)>>,
    /// 已交给后台线程、结果还没回来的路径。
    pending: RefCell<HashSet<PathBuf>>,
    incoming: RefCell<Option<Receiver<(PathBuf, String, String)>>>,
    outgoing: RefCell<Option<Sender<(PathBuf, String, String)>>>,
    /// 最近一次读到的模块（版本信息回来后据此重绘）。
    last: RefCell<Vec<Module>>,
    handles: RefCell<HandlesState>,
}

/// “句柄”页签的状态：只在页签打开期间有内容，离开页签即清空并通知名称工作线程停止。
#[derive(Default)]
struct HandlesState {
    list: Vec<Handle>,
    /// （句柄值，类型索引）→ 名称文字；空串表示没有名称。
    names: HashMap<(u64, u32), String>,
    /// 已交给工作线程的句柄，不重复请求。
    requested: HashSet<(u64, u32)>,
    rx: Option<Receiver<((u64, u32), String)>>,
    cancel: Option<Arc<AtomicBool>>,
}

/// 句柄名称查询的单次超时。
const NAME_TIMEOUT: Duration = Duration::from_millis(250);

impl super::Controller {
    pub(super) fn bind_process_details(self: &Rc<Self>, ui: &AppWindow) {
        let c = Rc::downgrade(self);
        ui.global::<ProcessMenu>().on_show_details(move || {
            if let Some(c) = c.upgrade()
                && let Some((key, name, _)) = c.selected_process()
            {
                c.open_details(key, name);
            }
        });
    }

    /// 打开（或换目标）详情窗口。
    pub(super) fn open_details(self: &Rc<Self>, key: ProcessKey, name: String) {
        let d = &self.details;
        if d.window.borrow().is_none() {
            let w = match ProcessDetailsWindow::new() {
                Ok(w) => w,
                Err(e) => {
                    self.log.borrow_mut().line(format!("详情窗口创建失败 {e}"));
                    return;
                }
            };
            let theme = self.settings.borrow().theme;
            if theme != 0 {
                w.invoke_set_theme(theme);
            }
            if let Some(s) = self.settings.borrow().details_window {
                w.set_widths(ModelRc::new(VecModel::from(s.widths.iter().map(|v| *v as f32).collect::<Vec<_>>())));
                w.window().set_size(slint::LogicalSize::new(s.width, s.height));
            }
            let c = Rc::downgrade(self);
            w.on_elevate(move || {
                if let Some(c) = c.upgrade() {
                    c.elevate();
                }
            });
            let c = Rc::downgrade(self);
            w.on_tab_changed(move || {
                if let Some(c) = c.upgrade() {
                    c.refresh_details();
                }
            });
            let c = Rc::downgrade(self);
            w.on_filter_changed(move || {
                if let Some(c) = c.upgrade()
                    && let Some(w) = c.details.window.borrow().as_ref()
                {
                    c.show_handles(w);
                }
            });
            let c = Rc::downgrade(self);
            w.on_resized_column(move || {
                if let Some(c) = c.upgrade() {
                    c.remember_details_layout();
                }
            });
            let c = Rc::downgrade(self);
            w.window().on_close_requested(move || {
                if let Some(c) = c.upgrade() {
                    c.close_details();
                }
                slint::CloseRequestResponse::HideWindow
            });
            *d.window.borrow_mut() = Some(w);
        }
        d.target.set(Some(key));
        *d.name.borrow_mut() = name;
        d.last.borrow_mut().clear();
        self.log.borrow_mut().line(format!("详情窗口 打开 pid={}", key.pid));
        self.refresh_details();
        if let Some(w) = d.window.borrow().as_ref() {
            let _ = w.show();
        }
        let c = Rc::downgrade(self);
        d.timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
            if let Some(c) = c.upgrade() {
                c.refresh_details();
            }
        });
    }

    /// 测试钩子 `--menu-action details:<pid>` 用：把窗口里的模块行数与已读到描述的行数写进日志，可选保存截图。
    pub fn details_probe(&self, shot: Option<&std::path::Path>) {
        let d = &self.details;
        let guard = d.window.borrow();
        let Some(w) = guard.as_ref() else {
            eprintln!("详情窗口 未打开");
            return;
        };
        let rows = w.get_modules();
        let described = (0..rows.row_count()).filter(|i| rows.row_data(*i).is_some_and(|r| !r.description.is_empty())).count();
        eprintln!("详情窗口 模块行数={} 有描述={} 错误={}", rows.row_count(), described, w.get_error());
        let threads = w.get_threads();
        let located = (0..threads.row_count()).filter(|i| threads.row_data(*i).is_some_and(|r| r.start.contains("+0x"))).count();
        eprintln!("详情窗口 线程行数={} 起始地址带偏移={}", threads.row_count(), located);
        let hs = w.get_handles();
        let named = (0..hs.row_count()).filter(|i| hs.row_data(*i).is_some_and(|r| !r.name.is_empty())).count();
        eprintln!("详情窗口 句柄行数={} 有名称={}", hs.row_count(), named);
        if let (Some(path), Ok(buf)) = (shot, w.window().take_snapshot()) {
            let _ = crate::screenshot::write_bmp(path, &buf);
        }
    }

    /// 测试钩子用：切到指定页签（1 线程、2 句柄）并立即刷新。
    pub fn details_select_tab(self: &Rc<Self>, tab: i32) {
        if let Some(w) = self.details.window.borrow().as_ref() {
            w.set_tab(tab);
        }
        self.refresh_details();
    }

    fn close_details(&self) {
        let d = &self.details;
        d.timer.stop();
        d.target.set(None);
        *d.incoming.borrow_mut() = None;
        *d.outgoing.borrow_mut() = None;
        d.pending.borrow_mut().clear();
        d.versions.borrow_mut().clear();
        d.last.borrow_mut().clear();
        self.stop_handles();
        self.remember_details_layout();
        self.log.borrow_mut().line("详情窗口 关闭".to_string());
    }

    /// 把窗口大小与列宽写进设置。
    fn remember_details_layout(&self) {
        let d = &self.details;
        let guard = d.window.borrow();
        let Some(w) = guard.as_ref() else { return };
        let size = w.window().size().to_logical(w.window().scale_factor());
        let model = w.get_widths();
        let mut widths = [0f32; 5];
        for (i, slot) in widths.iter_mut().enumerate() {
            *slot = model.row_data(i).unwrap_or(100.0);
        }
        self.settings.borrow_mut().details_window =
            Some(DetailsWindow { width: size.width, height: size.height, widths });
        self.save_settings();
    }

    /// 重读模块列表并刷新窗口。
    fn refresh_details(self: &Rc<Self>) {
        let d = &self.details;
        let Some(key) = d.target.get() else { return };
        let guard = d.window.borrow();
        let Some(w) = guard.as_ref() else { return };
        self.drain_versions();
        w.set_heading(trf("{0}（PID {1}）", &[&*d.name.borrow(), &key.pid]).into());
        if w.get_tab() != 2 {
            self.stop_handles();
        }
        if w.get_tab() == 1 {
            self.refresh_threads(w, key);
            return;
        }
        if w.get_tab() == 2 {
            self.refresh_handles(w, key);
            return;
        }
        match modules::list_modules(key) {
            Ok(mods) => {
                w.set_error("".into());
                w.set_needs_elevation(false);
                w.set_summary(trf("{0} 个模块", &[&mods.len()]).into());
                self.request_versions(&mods);
                *d.last.borrow_mut() = mods;
            }
            Err(e) => {
                if d.last.borrow().is_empty() {
                    self.log.borrow_mut().line(format!("详情窗口 模块读取失败 pid={} 错误码={}", key.pid, e.code()));
                }
                w.set_error(texts::module_error(&e).into());
                w.set_needs_elevation(e.needs_elevation());
                w.set_summary("".into());
                d.last.borrow_mut().clear();
            }
        }
        self.show_modules(w);
    }

    /// “线程”页签：读线程列表，起始地址用模块列表解析成“模块名+偏移”。
    fn refresh_threads(&self, w: &ProcessDetailsWindow, key: ProcessKey) {
        use tml_core::threads;
        match threads::list_threads(key) {
            Ok(list) => {
                let mods = modules::list_modules(key).unwrap_or_default();
                let rows: Vec<ThreadRow> = list
                    .iter()
                    .map(|t| ThreadRow {
                        tid: t.tid.to_string().into(),
                        priority: format!("{}", t.priority).into(),
                        state: texts::thread_state(t.state, t.wait_reason).into(),
                        cpu: crate::details::cpu_time(t.cpu_time).into(),
                        start: match threads::locate(t.start_address, &mods) {
                            Some((name, off)) => format!("{name}+0x{off:X}"),
                            None => format!("0x{:X}", t.start_address),
                        }
                        .into(),
                        switches: t.context_switches.to_string().into(),
                    })
                    .collect();
                w.set_error("".into());
                w.set_needs_elevation(false);
                w.set_summary(trf("{0} 个线程", &[&rows.len()]).into());
                w.set_threads(ModelRc::new(VecModel::from(rows)));
            }
            Err(e) => {
                self.log.borrow_mut().line(format!("详情窗口 线程读取失败 pid={} 错误码={}", key.pid, e.code()));
                w.set_error(texts::thread_error(&e).into());
                w.set_needs_elevation(false);
                w.set_summary("".into());
                w.set_threads(ModelRc::new(VecModel::from(Vec::<ThreadRow>::new())));
            }
        }
    }

    /// 停止名称工作线程并清空句柄状态（离开页签、关闭窗口时调用）。
    fn stop_handles(&self) {
        let mut st = self.details.handles.borrow_mut();
        if let Some(c) = st.cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        *st = HandlesState::default();
    }

    /// “句柄”页签：列出句柄，名称交给后台线程逐个带超时地查，结果在后续节拍里补上。
    fn refresh_handles(&self, w: &ProcessDetailsWindow, key: ProcessKey) {
        let d = &self.details;
        match handles::list_handles(key) {
            Ok(list) => {
                w.set_error("".into());
                w.set_needs_elevation(false);
                let mut st = d.handles.borrow_mut();
                let alive: HashSet<(u64, u32)> = list.iter().map(|h| (h.value, h.type_index)).collect();
                st.names.retain(|k, _| alive.contains(k));
                st.requested.retain(|k| alive.contains(k));
                // 取回名称线程已查到的结果；线程结束（通道断开）后允许再起一个，补查新出现的句柄。
                if let Some(rx) = st.rx.take() {
                    let mut done = false;
                    loop {
                        match rx.try_recv() {
                            Ok((k, n)) => {
                                if alive.contains(&k) {
                                    st.names.insert(k, n);
                                }
                            }
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => {
                                done = true;
                                break;
                            }
                        }
                    }
                    if !done {
                        st.rx = Some(rx);
                    }
                }
                st.list = list;
                if st.rx.is_none() {
                    let todo: Vec<Handle> = st
                        .list
                        .iter()
                        .filter(|h| !h.type_name.is_empty() && !st.requested.contains(&(h.value, h.type_index)))
                        .cloned()
                        .collect();
                    if !todo.is_empty()
                        && let Ok(mut namer) = HandleNamer::open(key)
                    {
                        for h in &todo {
                            st.requested.insert((h.value, h.type_index));
                        }
                        let no_name: String = tr("（无法获取名称）").into();
                        let (tx, rx) = mpsc::channel();
                        let cancel = Arc::new(AtomicBool::new(false));
                        st.rx = Some(rx);
                        st.cancel = Some(cancel.clone());
                        let _ = std::thread::Builder::new().name("handle-names".into()).spawn(move || {
                            for h in todo {
                                if cancel.load(Ordering::Relaxed) {
                                    break;
                                }
                                let text = match namer.name(&h, NAME_TIMEOUT) {
                                    NameResult::Name(n) => n,
                                    NameResult::TimedOut => no_name.clone(),
                                    NameResult::Unnamed | NameResult::Failed => String::new(),
                                };
                                if tx.send(((h.value, h.type_index), text)).is_err() {
                                    break;
                                }
                            }
                        });
                    }
                }
                drop(st);
                self.show_handles(w);
            }
            Err(e) => {
                self.log.borrow_mut().line(format!("详情窗口 句柄读取失败 pid={} 错误码={}", key.pid, e.code()));
                self.stop_handles();
                w.set_error(texts::handle_error(&e).into());
                w.set_needs_elevation(e.needs_elevation());
                w.set_summary("".into());
                w.set_handles(ModelRc::new(VecModel::from(Vec::<HandleRow>::new())));
            }
        }
    }

    /// 按类型过滤后重建句柄行。
    fn show_handles(&self, w: &ProcessDetailsWindow) {
        let st = self.details.handles.borrow();
        let filter = w.get_filter().to_lowercase();
        let rows: Vec<HandleRow> = st
            .list
            .iter()
            .filter(|h| filter.is_empty() || h.type_name.to_lowercase().contains(&filter))
            .map(|h| HandleRow {
                kind: if h.type_name.is_empty() { format!("#{}", h.type_index) } else { h.type_name.clone() }.into(),
                name: st.names.get(&(h.value, h.type_index)).cloned().unwrap_or_default().into(),
                value: format!("0x{:X}", h.value).into(),
                access: format!("0x{:08X}", h.access).into(),
            })
            .collect();
        w.set_summary(
            if filter.is_empty() {
                trf("{0} 个句柄", &[&rows.len()])
            } else {
                trf("{0} / {1} 个句柄", &[&rows.len(), &st.list.len()])
            }
            .into(),
        );
        w.set_handles(ModelRc::new(VecModel::from(rows)));
    }

    fn show_modules(&self, w: &ProcessDetailsWindow) {
        let d = &self.details;
        let versions = d.versions.borrow();
        let rows: Vec<ModuleRow> = d
            .last
            .borrow()
            .iter()
            .map(|m| {
                let (description, company) = versions.get(&m.path).cloned().unwrap_or_default();
                ModuleRow {
                    name: m.name.clone().into(),
                    path: m.path.display().to_string().into(),
                    base: format!("0x{:X}", m.base).into(),
                    size: format_size(m.size).into(),
                    description: description.into(),
                    company: company.into(),
                }
            })
            .collect();
        w.set_modules(ModelRc::new(VecModel::from(rows)));
    }

    /// 取回后台线程读到的描述 / 公司。
    fn drain_versions(&self) {
        let d = &self.details;
        let guard = d.incoming.borrow();
        let Some(rx) = guard.as_ref() else { return };
        while let Ok((path, description, company)) = rx.try_recv() {
            d.pending.borrow_mut().remove(&path);
            d.versions.borrow_mut().insert(path, (description, company));
        }
    }

    /// 还没读过版本信息的路径交给一个一次性后台线程。
    fn request_versions(&self, mods: &[Module]) {
        let d = &self.details;
        let todo: Vec<PathBuf> = {
            let versions = d.versions.borrow();
            let mut pending = d.pending.borrow_mut();
            mods.iter()
                .filter(|m| !versions.contains_key(&m.path) && pending.insert(m.path.clone()))
                .map(|m| m.path.clone())
                .collect()
        };
        if todo.is_empty() {
            return;
        }
        if d.outgoing.borrow().is_none() {
            let (tx, rx) = mpsc::channel();
            *d.incoming.borrow_mut() = Some(rx);
            *d.outgoing.borrow_mut() = Some(tx);
        }
        let tx = d.outgoing.borrow().clone().expect("通道刚创建");
        let _ = std::thread::Builder::new().name("module-versions".into()).spawn(move || {
            for path in todo {
                let (description, company) = modules::file_version_strings(&path);
                if tx.send((path, description.unwrap_or_default(), company.unwrap_or_default())).is_err() {
                    break;
                }
            }
        });
    }
}

fn format_size(bytes: u32) -> String {
    let kb = (bytes as f64 / 1024.0).ceil() as u64;
    format!("{} KB", kb)
}

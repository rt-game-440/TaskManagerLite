# 0007 界面语言策略

状态：已接受（2026-10-31），依赖 #87 的翻译机制。

## 背景

#87 让界面文案可翻译；#89 增加语言设置与系统语言检测。

## 决定

- **key 为中文原文**：Slint 用 `@tr("中文")`，Rust 用 `i18n::tr("中文")` / `trf`；中文界面不查表，直接用原文。英文是翻译：Slint 在 `crates/app/lang/en/LC_MESSAGES/tml-app.po`（slint-build 编进可执行文件，默认翻译上下文关闭），Rust 在 `crates/app/lang/en-rust.json`。
- **新增文案的流程**：写中文原文 → `python scripts/i18n.py extract` → 补英文 → `python scripts/i18n.py check`（`scripts/test-i18n.ps1` 也会跑）。日志里的中文不翻译。
- **设置字段 `language`**：`system`（默认，缺字段也按此）/ `zh` / `en`。跟随系统读 Windows 首选显示语言（`GetUserPreferredUILanguages`），`zh-*` 用中文，其他一律英文；运行期间系统语言变化不监听，下次启动生效。
- **优先级**：命令行 `--lang <键>` 与环境变量 `TML_LANG`（截图、测试用）高于设置；`--screenshot` 等开发模式不读设置。
- **切换立即生效**：设置页切换后调用 `i18n::set_lang`，Slint 文案由内置翻译刷新，托盘菜单每次弹出时现取文案，托盘提示与进程列表随即重算。
- **数字与单位**：速率单位（KB/s、MB/s、Mbps）、百分比、小数点在两种语言下一致，不使用系统区域设置；`format.rs` 无需随语言变化。
- **安装包**：不写初始 `language`，保持“跟随系统”，避免安装语言与界面语言互相意外影响。

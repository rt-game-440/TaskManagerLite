# 0008 检查更新（一期）

状态：已接受（2026-11-02），对应 #99；下载与安装（二期）见 #100，需另写 ADR。

## 背景

用户需要知道有没有新版本。项目之前完全不联网，常驻开销和隐私是卖点，所以联网方式要克制、可解释。

## 决定

- **只在用户点击“检查更新”时联网一次**：不自动检查、不定时轮询、不在启动时查询，也不上报任何数据。请求只带 `User-Agent: TaskManagerLite/<版本>`。
- **数据源**：GitHub REST `GET /repos/rt-game-440/TaskManagerLite/releases/latest`。该接口只返回最新的正式版（不含预发布与草稿），所以一期没有“包含预发布”开关。匿名调用每小时 60 次的限额对手动检查足够；403 / 429 显示“请求过于频繁”。
- **版本比较**：按 SemVer 2.0 优先级（core 的 `update::Version`）。当前版本取编译期 `CARGO_PKG_VERSION`（工作区版本，与发布 tag 一致）。当前版本高于最新正式版（开发版、预发布版）时显示“已是最新”，不提示降级；同号的预发布版低于正式版，会提示更新。
- **网络栈用系统自带的 WinHTTP**（`windows` crate 已有，只多开一个 feature）：不引入 reqwest / ureq / TLS 库，二进制体积和依赖树不变；遵循系统代理设置（`WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY`），TLS 与证书校验交给系统。解析、连接、发送、接收各 10 秒超时，响应体上限 1 MiB。JSON 解析用工作区已有的 `serde_json`。
- **后台线程**：查询在一次性线程里执行，结果经 `slint::invoke_from_event_loop` 回到界面线程；查询期间按钮禁用并显示“正在检查…”。
- **只打开可信地址**：“打开 Releases 页面”只会打开 `https://github.com/rt-game-440/TaskManagerLite/releases` 或其下的地址。响应里的 `html_url` 不以该前缀开头时退回 Releases 列表，避免被篡改的响应把用户引到别处。
- **错误结构化**：`UpdateError`（超时 / 连不上 / 安全连接失败 / 其他网络错误 / 无正式发布 / 限流 / 其他 HTTP 状态 / 响应无法识别 / 标签不是版本号），文案在 `texts.rs::update_error`。日志只记结果、版本号、错误码和耗时，不记发布说明正文。
- **可测试**：环境变量 `TML_UPDATE_URL` 改写查询地址；`--check-update` 测试钩子查询一次、写日志后退出。`scripts/test-update.ps1` 在 localhost 起模拟服务覆盖各场景，不访问真实网络。

## 不做（一期）

下载、校验、安装、自动检查、预发布通道、跳过某个版本。

## 后果

- 程序第一次具备联网能力，但只在用户明确操作时发生；防火墙或代理拦截时只影响这一个功能，并给出可理解的原因。
- 仓库改名或迁移时要同步改 `crates/core/src/update.rs` 里的两个地址常量。

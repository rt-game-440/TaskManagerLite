//! 检查更新（#99 一期）：向 GitHub Releases 查询最新正式版，与当前版本按语义化版本比较。
//!
//! 只在用户点击“检查更新”时联网一次（见 ADR 0008）；不下载、不安装。网络请求用系统自带的
//! WinHTTP（遵循系统代理设置），响应的解析与比较是纯函数，可脱离网络单测。

use crate::error::SystemError;
use std::cmp::Ordering;
use std::fmt;
use windows::Win32::Networking::WinHttp::{
    URL_COMPONENTS, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ADDREQ_FLAG_ADD, WINHTTP_FLAG_SECURE,
    WINHTTP_INTERNET_SCHEME_HTTPS, WINHTTP_OPEN_REQUEST_FLAGS, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
    WinHttpAddRequestHeaders, WinHttpCloseHandle, WinHttpConnect, WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
};
use windows::core::{PCWSTR, w};

/// 查询最新正式版的接口（`/releases/latest` 不含预发布与草稿）。
pub const LATEST_RELEASE_API: &str = "https://api.github.com/repos/rt-game-440/TaskManagerLite/releases/latest";
/// Releases 页面；响应里的发布页地址不可信时也退回这里。
pub const RELEASES_PAGE: &str = "https://github.com/rt-game-440/TaskManagerLite/releases";
/// 自动化验收：改写查询地址（指向本地模拟服务，不访问真实网络）。
pub const UPDATE_URL_ENV: &str = "TML_UPDATE_URL";

/// 响应体上限：GitHub 的单个发布信息远小于此，超过视为异常响应。
const MAX_BODY: usize = 1 << 20;

/// 语义化版本（SemVer 2.0）：主.次.修订，可带预发布标识；构建元数据（`+…`）解析时忽略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// 预发布标识（`-` 之后按 `.` 分段）；为空表示正式版。
    pub pre: Vec<String>,
}

impl Version {
    /// 解析 `1.2.3`、`v1.2.3-rc.1+build`；不合法时为 `None`。
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (text, None),
        };
        let mut nums = core.split('.').map(|n| {
            let ok = !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && (n == "0" || !n.starts_with('0'));
            ok.then(|| n.parse::<u64>().ok()).flatten()
        });
        let (major, minor, patch) = (nums.next()??, nums.next()??, nums.next()??);
        if nums.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(p) => {
                let ids: Vec<String> = p.split('.').map(str::to_string).collect();
                let valid = |id: &String| {
                    !id.is_empty()
                        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        && !(id.len() > 1 && id.starts_with('0') && id.bytes().all(|b| b.is_ascii_digit()))
                };
                if !ids.iter().all(valid) {
                    return None;
                }
                ids
            }
        };
        Some(Self { major, minor, patch, pre })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch)).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // 正式版高于同号的预发布版。
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let ord = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            // 纯数字标识低于含字母的标识。
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

/// 一个发布（来自 GitHub Releases 的响应）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// 原始标签，如 `v0.4.0`。
    pub tag: String,
    /// 发布标题（可能为空）。
    pub name: String,
    /// 发布说明（Markdown 原文）。
    pub notes: String,
    /// 发布页地址；只接受本仓库在 github.com 上的地址，否则为 [`RELEASES_PAGE`]。
    pub url: String,
}

/// 检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// 当前版本不低于最新正式版（含当前为更新的预发布版、开发版）。
    UpToDate { latest: Version },
    /// 有更新的正式版。
    Available(Release),
}

/// 检查更新失败的原因。界面层按枚举映射文案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    /// 当前程序的版本号不合法（构建配置问题）。
    BadCurrentVersion(String),
    /// 连接或读取超时。
    Timeout,
    /// 无法解析域名或连不上服务器（多为没有联网、被防火墙或代理拦截），带 WinHTTP 错误码。
    Unreachable(i32),
    /// 安全连接（TLS / 证书）失败，带 WinHTTP 错误码。
    Secure(i32),
    /// 其他网络错误。
    Network(SystemError),
    /// 仓库还没有正式发布（HTTP 404）。
    NoRelease,
    /// 访问过于频繁被 GitHub 限流（HTTP 403 / 429）。
    RateLimited,
    /// 其他 HTTP 状态码。
    Http(u16),
    /// 响应不是预期的发布信息（JSON 不合法、缺少标签、超出大小上限）。
    BadResponse,
    /// 最新发布的标签不是合法的版本号。
    BadTag(String),
}

impl UpdateError {
    /// 写日志用的错误码（WinHTTP / Win32 错误码或 HTTP 状态码）。
    pub fn code(&self) -> Option<i32> {
        match self {
            Self::Unreachable(c) | Self::Secure(c) => Some(*c),
            Self::Network(e) => e.win32_code(),
            Self::Http(s) => Some(i32::from(*s)),
            _ => None,
        }
    }
}

/// 解析 `/releases/latest` 的响应体并与当前版本比较（纯函数）。
pub fn evaluate(current: &str, body: &[u8]) -> Result<UpdateStatus, UpdateError> {
    let current = Version::parse(current).ok_or_else(|| UpdateError::BadCurrentVersion(current.to_string()))?;
    let release = parse_release(body)?;
    Ok(if release.version > current {
        UpdateStatus::Available(release)
    } else {
        UpdateStatus::UpToDate { latest: release.version }
    })
}

fn parse_release(body: &[u8]) -> Result<Release, UpdateError> {
    let json: serde_json::Value = serde_json::from_slice(body).map_err(|_| UpdateError::BadResponse)?;
    let text = |key: &str| json.get(key).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
    let tag = text("tag_name");
    if tag.is_empty() {
        return Err(UpdateError::BadResponse);
    }
    let version = Version::parse(&tag).ok_or_else(|| UpdateError::BadTag(tag.clone()))?;
    let url = text("html_url");
    let url = if url.starts_with(&format!("{RELEASES_PAGE}/")) { url } else { RELEASES_PAGE.to_string() };
    Ok(Release { version, tag, name: text("name"), notes: text("body"), url })
}

/// 发布说明摘要：去掉空行与 Markdown 标题、列表标记，最多 `max_lines` 行、`max_chars` 个字符，截断时加“…”。
pub fn notes_summary(notes: &str, max_lines: usize, max_chars: usize) -> String {
    let lines: Vec<&str> = notes
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim_start_matches(['-', '*']).trim())
        .filter(|l| !l.is_empty())
        .collect();
    let mut out = lines.iter().take(max_lines).copied().collect::<Vec<_>>().join("\n");
    let mut cut = lines.len() > max_lines;
    if out.chars().count() > max_chars {
        out = out.chars().take(max_chars).collect::<String>().trim_end().to_string();
        cut = true;
    }
    if cut {
        out.push('…');
    }
    out
}

/// 查询地址：环境变量 [`UPDATE_URL_ENV`] 优先，否则为 [`LATEST_RELEASE_API`]。
pub fn update_url() -> String {
    std::env::var(UPDATE_URL_ENV).ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| LATEST_RELEASE_API.to_string())
}

/// 联网检查一次（阻塞，放在后台线程调用）。
pub fn check_for_update(current: &str, url: &str) -> Result<UpdateStatus, UpdateError> {
    // 先校验当前版本，免得白白联网。
    Version::parse(current).ok_or_else(|| UpdateError::BadCurrentVersion(current.to_string()))?;
    let body = http_get(url, current)?;
    evaluate(current, &body)
}

/// 关闭 WinHTTP 句柄。
struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn last_error() -> UpdateError {
    net_error(windows::core::Error::from_thread())
}

fn net_error(e: windows::core::Error) -> UpdateError {
    let hr = e.code().0;
    // HRESULT_FROM_WIN32：低 16 位是 Win32 / WinHTTP 错误码。
    let code = if (hr as u32) & 0xFFFF_0000 == 0x8007_0000 { (hr & 0xFFFF) as i32 } else { hr };
    match code {
        12002 => UpdateError::Timeout,
        // 12007 无法解析域名，12029 无法连接，12030 连接被中断，12031 连接被重置。
        12007 | 12029 | 12030 | 12031 => UpdateError::Unreachable(code),
        // 12175 安全连接失败，12037–12045 证书问题，12057 吊销检查失败。
        12175 | 12037..=12045 | 12057 => UpdateError::Secure(code),
        _ => UpdateError::Network(SystemError::Win32(code)),
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 用 WinHTTP 发一个 GET 请求，返回 200 响应的正文。
fn http_get(url: &str, version: &str) -> Result<Vec<u8>, UpdateError> {
    let url_w: Vec<u16> = url.encode_utf16().collect();
    let mut parts = URL_COMPONENTS {
        dwStructSize: size_of::<URL_COMPONENTS>() as u32,
        dwHostNameLength: u32::MAX,
        dwUrlPathLength: u32::MAX,
        dwExtraInfoLength: u32::MAX,
        ..Default::default()
    };
    unsafe { WinHttpCrackUrl(&url_w, 0, &mut parts) }.map_err(|_| UpdateError::Network(SystemError::Win32(87)))?;
    let slice = |p: windows::core::PWSTR, len: u32| -> String {
        if p.is_null() { String::new() } else { String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p.0, len as usize) }) }
    };
    let host = wide(&slice(parts.lpszHostName, parts.dwHostNameLength));
    let mut object = slice(parts.lpszUrlPath, parts.dwUrlPathLength);
    object.push_str(&slice(parts.lpszExtraInfo, parts.dwExtraInfoLength));
    if object.is_empty() {
        object.push('/');
    }
    let object = wide(&object);
    let secure = parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS;

    let agent = wide(&format!("TaskManagerLite/{version}"));
    let session = Handle(unsafe {
        WinHttpOpen(PCWSTR(agent.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0)
    });
    if session.0.is_null() {
        return Err(last_error());
    }
    // 解析、连接、发送、接收各 10 秒。
    unsafe { WinHttpSetTimeouts(session.0, 10_000, 10_000, 10_000, 10_000) }.map_err(net_error)?;
    let connect = Handle(unsafe { WinHttpConnect(session.0, PCWSTR(host.as_ptr()), parts.nPort, 0) });
    if connect.0.is_null() {
        return Err(last_error());
    }
    let flags = if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
    let request = Handle(unsafe {
        WinHttpOpenRequest(connect.0, w!("GET"), PCWSTR(object.as_ptr()), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), flags)
    });
    if request.0.is_null() {
        return Err(last_error());
    }
    // User-Agent（GitHub 接口要求）由会话的 agent 字符串提供。
    let headers: Vec<u16> = "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28".encode_utf16().collect();
    unsafe { WinHttpAddRequestHeaders(request.0, &headers, WINHTTP_ADDREQ_FLAG_ADD) }.map_err(net_error)?;
    unsafe { WinHttpSendRequest(request.0, None, None, 0, 0, 0) }.map_err(net_error)?;
    unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) }.map_err(net_error)?;

    let mut status = 0u32;
    let mut len = size_of::<u32>() as u32;
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut status).cast()),
            &mut len,
            std::ptr::null_mut(),
        )
    }
    .map_err(net_error)?;
    match status {
        200 => {}
        404 => return Err(UpdateError::NoRelease),
        403 | 429 => return Err(UpdateError::RateLimited),
        s => return Err(UpdateError::Http(u16::try_from(s).unwrap_or(u16::MAX))),
    }

    let mut body = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let mut read = 0u32;
        unsafe { WinHttpReadData(request.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut read) }.map_err(net_error)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buf[..read as usize]);
        if body.len() > MAX_BODY {
            return Err(UpdateError::BadResponse);
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s} 应可解析"))
    }

    #[test]
    fn parses_versions_and_rejects_invalid() {
        assert_eq!(v("v0.3.0"), Version { major: 0, minor: 3, patch: 0, pre: vec![] });
        assert_eq!(v("1.2.3-rc.1+build.5").pre, ["rc", "1"]);
        assert_eq!(v(" V10.20.30 ").to_string(), "10.20.30");
        for bad in ["", "1.2", "1.2.3.4", "01.2.3", "1.2.x", "1.2.3-", "1.2.3-01", "1.2.3-a..b", "v"] {
            assert_eq!(Version::parse(bad), None, "{bad:?} 不应解析");
        }
    }

    #[test]
    fn orders_by_semver_precedence() {
        // SemVer 2.0 规范里的示例顺序。
        let chain = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.1.0",
            "2.0.0",
        ];
        for pair in chain.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(v("1.0.0+a").cmp(&v("1.0.0+b")), Ordering::Equal, "构建元数据不参与比较");
        assert!(v("0.10.0") > v("0.9.9"), "按数字而不是字符串比较");
    }

    fn release_json(tag: &str, url: &str) -> Vec<u8> {
        serde_json::json!({ "tag_name": tag, "name": "TaskManagerLite 0.4.0", "body": "## 新功能\n- 表头总占用", "html_url": url })
            .to_string()
            .into_bytes()
    }

    #[test]
    fn evaluate_reports_newer_release_or_up_to_date() {
        let url = format!("{RELEASES_PAGE}/tag/v0.4.0");
        match evaluate("0.3.0", &release_json("v0.4.0", &url)) {
            Ok(UpdateStatus::Available(r)) => {
                assert_eq!((r.version.to_string(), r.tag.as_str(), r.url.as_str()), ("0.4.0".into(), "v0.4.0", url.as_str()));
                assert!(r.notes.contains("表头总占用"));
            }
            other => panic!("应发现新版本，实际 {other:?}"),
        }
        assert_eq!(evaluate("0.4.0", &release_json("v0.4.0", &url)), Ok(UpdateStatus::UpToDate { latest: v("0.4.0") }));
        // 当前是更新的预发布版或开发版：不提示“降级”。
        assert_eq!(evaluate("0.5.0-beta.1", &release_json("v0.4.0", &url)), Ok(UpdateStatus::UpToDate { latest: v("0.4.0") }));
        // 当前是同号预发布版：正式版更新。
        assert!(matches!(evaluate("0.4.0-rc.2", &release_json("v0.4.0", &url)), Ok(UpdateStatus::Available(_))));
    }

    #[test]
    fn evaluate_rejects_bad_responses() {
        assert_eq!(evaluate("0.3.0", b"<html>"), Err(UpdateError::BadResponse));
        assert_eq!(evaluate("0.3.0", br#"{"name":"x"}"#), Err(UpdateError::BadResponse));
        assert_eq!(evaluate("0.3.0", &release_json("nightly", RELEASES_PAGE)), Err(UpdateError::BadTag("nightly".into())));
        assert_eq!(evaluate("dev", &release_json("v1.0.0", RELEASES_PAGE)), Err(UpdateError::BadCurrentVersion("dev".into())));
    }

    #[test]
    fn untrusted_release_url_falls_back_to_releases_page() {
        for url in ["https://evil.example/x", "http://github.com/rt-game-440/TaskManagerLite/releases/tag/v1.0.0", "", "https://github.com/rt-game-440/TaskManagerLite/releasesX"] {
            let Ok(UpdateStatus::Available(r)) = evaluate("0.1.0", &release_json("v1.0.0", url)) else { panic!() };
            assert_eq!(r.url, RELEASES_PAGE, "{url:?}");
        }
    }

    #[test]
    fn notes_summary_strips_markup_and_truncates() {
        let notes = "## 新功能\r\n\r\n- 表头总占用\r\n* 左栏可拖动\r\n- 连接页图标\r\n- 检查更新";
        assert_eq!(notes_summary(notes, 3, 200), "新功能\n表头总占用\n左栏可拖动…");
        assert_eq!(notes_summary(notes, 10, 8), "新功能\n表头总占…");
        assert_eq!(notes_summary("", 3, 100), "");
        assert_eq!(notes_summary("一行", 3, 100), "一行");
    }

    #[test]
    fn maps_winhttp_errors() {
        let e = |code: u32| net_error(windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(code)));
        assert_eq!(e(12002), UpdateError::Timeout);
        assert_eq!(e(12007), UpdateError::Unreachable(12007));
        assert_eq!(e(12029).code(), Some(12029));
        assert_eq!(e(12175), UpdateError::Secure(12175));
        assert_eq!(e(5), UpdateError::Network(SystemError::Win32(5)));
        assert_eq!(UpdateError::Http(502).code(), Some(502));
    }
}

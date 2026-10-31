//! 界面文案的翻译入口（issue #87）。
//!
//! 约定：中文原文就是 key，中文界面不经过任何查表；英文界面按 `lang/en-rust.json`（Rust 文案）
//! 与 `lang/en/LC_MESSAGES/tml-app.po`（Slint `@tr`，由 slint-build 编进可执行文件）翻译。
//! 带参数的文案用占位符 `{0}`、`{1}`（或按顺序的 `{}`），译文可以调整占位符顺序，不拼接句子。
//! 日志里的中文不翻译。新增文案后运行 `python scripts/i18n.py extract` 同步模板，
//! `python scripts/i18n.py check` 检查漏翻译。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);
static EN_TABLE: OnceLock<HashMap<String, &'static str>> = OnceLock::new();

/// 界面语言。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    /// 语言键（`zh`、`en`，带地区后缀如 `en-US` 也认）；认不出时为 `None`。
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        match s.split(['-', '_']).next()? {
            "zh" => Some(Self::Zh),
            "en" => Some(Self::En),
            _ => None,
        }
    }
}

/// 当前语言。
pub fn lang() -> Lang {
    if ENGLISH.load(Ordering::Relaxed) { Lang::En } else { Lang::Zh }
}

/// 选择语言：Rust 文案立即生效，Slint 文案通过内置翻译切换。
pub fn set_lang(l: Lang) {
    ENGLISH.store(l == Lang::En, Ordering::Relaxed);
    apply_slint();
}

/// 把当前语言应用到 Slint 内置翻译；Slint 要求在创建第一个组件之后调用，所以创建窗口后须再调一次。
pub fn apply_slint() {
    let _ = slint::select_bundled_translation(if lang() == Lang::En { "en" } else { "" });
}

/// 启动时按 `--lang <键>` 或环境变量 `TML_LANG` 选择语言（语言设置项见本地化 3/3），默认中文。
pub fn init_from_env() {
    let mut args = std::env::args();
    let mut chosen = None;
    while let Some(a) = args.next() {
        if a == "--lang" {
            chosen = args.next();
        }
    }
    let chosen = chosen.or_else(|| std::env::var("TML_LANG").ok());
    if let Some(l) = chosen.as_deref().and_then(Lang::parse) {
        set_lang(l);
    }
}

fn table() -> &'static HashMap<String, &'static str> {
    EN_TABLE.get_or_init(|| {
        let raw: HashMap<String, String> = serde_json::from_str(include_str!("../lang/en-rust.json")).unwrap_or_default();
        // 译文只加载一次且数量有限（几百条），泄漏成 'static，让 tr() 可以像字面量一样返回 &'static str。
        raw.into_iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k, &*Box::leak(v.into_boxed_str()))).collect()
    })
}

/// 翻译一条 Rust 文案（中文原文作 key）；中文界面或查不到译文时原样返回。
pub fn tr(zh: &'static str) -> &'static str {
    if lang() == Lang::Zh {
        return zh;
    }
    table().get(zh).copied().unwrap_or(zh)
}

/// 翻译带参数的文案并代入：`{0}`、`{1}` 按下标，`{}` 按出现顺序。
pub fn trf(zh: &'static str, args: &[&dyn std::fmt::Display]) -> String {
    fill(tr(zh), args)
}

fn fill(template: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut next = 0;
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        let Some(j) = tail.find('}') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let spec = &tail[..j];
        let idx = if spec.is_empty() {
            next += 1;
            Some(next - 1)
        } else {
            spec.parse::<usize>().ok()
        };
        match idx.and_then(|n| args.get(n)) {
            Some(a) => out.push_str(&a.to_string()),
            None => out.push_str(&rest[i..i + j + 2]),
        }
        rest = &tail[j + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_language_keys() {
        assert_eq!(Lang::parse("en"), Some(Lang::En));
        assert_eq!(Lang::parse("EN-us"), Some(Lang::En));
        assert_eq!(Lang::parse("zh_CN"), Some(Lang::Zh));
        assert_eq!(Lang::parse("fr"), None);
    }

    #[test]
    fn fill_supports_indexed_sequential_and_reordered() {
        assert_eq!(fill("{0} 和 {1}", &[&"a", &2]), "a 和 2");
        assert_eq!(fill("{} / {}", &[&1, &2]), "1 / 2");
        assert_eq!(fill("{1} before {0}", &[&"a", &"b"]), "b before a");
        assert_eq!(fill("缺 {3} 参数", &[&1]), "缺 {3} 参数");
        assert_eq!(fill("没有闭合 {", &[]), "没有闭合 {");
    }

    #[test]
    fn chinese_returns_original() {
        assert_eq!(tr("任意文案"), "任意文案");
    }

    #[test]
    fn english_table_parses_and_has_no_empty_values() {
        let t = table();
        assert!(t.values().all(|v| !v.is_empty()));
    }
}

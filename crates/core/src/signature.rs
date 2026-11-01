//! 可执行文件的数字签名校验（#76，`WinVerifyTrust`）。
//!
//! 离线完成：不做吊销检查，也不联网取证书（`WTD_CACHE_ONLY_URL_RETRIEVAL`）。先校验文件内嵌签名，
//! 没有内嵌签名时按文件哈希查系统目录签名（catalog，系统文件大多如此）。
//! 核心库不返回显示文案，界面层按 [`Signature`] 映射文字。

use std::path::Path;
use std::sync::{Arc, OnceLock};

use windows::Win32::Foundation::{HANDLE, HWND, TRUST_E_NOSIGNATURE, TRUST_E_PROVIDER_UNKNOWN, TRUST_E_SUBJECT_FORM_UNKNOWN};
use windows::Win32::Security::Cryptography::Catalog::{
    CATALOG_INFO, CryptCATAdminAcquireContext2, CryptCATAdminCalcHashFromFileHandle2, CryptCATAdminEnumCatalogFromHash,
    CryptCATAdminReleaseCatalogContext, CryptCATAdminReleaseContext, CryptCATCatalogInfoFromContext,
};
use windows::Win32::Security::Cryptography::{CERT_NAME_SIMPLE_DISPLAY_TYPE, CertGetNameStringW};
use windows::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_CATALOG_INFO, WINTRUST_DATA, WINTRUST_FILE_INFO, WTD_CACHE_ONLY_URL_RETRIEVAL,
    WTD_CHOICE_CATALOG, WTD_CHOICE_FILE, WTD_REVOCATION_CHECK_NONE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTHelperGetProvCertFromChain, WTHelperGetProvSignerFromChain,
    WTHelperProvDataFromStateData, WinVerifyTrust,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::control::OwnedHandle;

/// 一个文件的签名校验结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signature {
    /// 签名有效且链条受信任；内容是签名者（证书主题的显示名，读不到时为空串）。
    Signed(String),
    /// 没有任何签名（内嵌与目录签名都没有）。
    Unsigned,
    /// 有签名但校验失败（文件被改、证书不受信任、文件读不到等）。
    Invalid,
}

impl Signature {
    /// 排序键：未签名 < 校验失败 < 已验证（再按签名者）。
    pub fn rank(&self) -> u8 {
        match self {
            Self::Unsigned => 1,
            Self::Invalid => 2,
            Self::Signed(_) => 3,
        }
    }
}

/// 后台校验结果的共享槽位：同一路径的所有进程共用一个，校验完成前为空（界面显示“…”）。
/// 相等性按槽位身份比较，不比较内容。
#[derive(Debug, Clone, Default)]
pub struct SigSlot(Arc<OnceLock<Signature>>);

impl SigSlot {
    pub fn get(&self) -> Option<&Signature> {
        self.0.get()
    }

    /// 写入结果，只有第一次生效。
    pub fn set(&self, sig: Signature) {
        let _ = self.0.set(sig);
    }
}

impl PartialEq for SigSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for SigSlot {}

fn wide0(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 校验 `path` 指向的文件。离线，单次耗时通常几毫秒到几十毫秒，调用方应放在低优先级后台线程。
pub fn verify(path: &Path) -> Signature {
    let file = unsafe {
        CreateFileW(
            &HSTRING::from(path.as_os_str()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map(OwnedHandle::from_raw);
    let Ok(file) = file else { return Signature::Invalid };
    let wpath = wide0(&path.to_string_lossy());

    let mut info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wpath.as_ptr()),
        hFile: file.raw(),
        ..Default::default()
    };
    let mut data = new_data(WTD_CHOICE_FILE);
    data.Anonymous.pFile = &mut info;
    let (status, name) = unsafe { run(&mut data) };
    if status == 0 {
        return Signature::Signed(name.unwrap_or_default());
    }
    let no_embedded = [TRUST_E_NOSIGNATURE.0, TRUST_E_PROVIDER_UNKNOWN.0, TRUST_E_SUBJECT_FORM_UNKNOWN.0];
    if !no_embedded.contains(&status) {
        return Signature::Invalid;
    }
    catalog_signature(file.raw(), &wpath)
}

fn new_data(choice: windows::Win32::Security::WinTrust::WINTRUST_DATA_UNION_CHOICE) -> WINTRUST_DATA {
    WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: choice,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_REVOCATION_CHECK_NONE,
        ..Default::default()
    }
}

/// 执行一次校验并取出签名者名，随后关闭状态。返回 `WinVerifyTrust` 的状态与签名者。
unsafe fn run(data: &mut WINTRUST_DATA) -> (i32, Option<String>) {
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // 无界面时 hwnd 传 INVALID_HANDLE_VALUE。
    let hwnd = HWND(-1isize as *mut core::ffi::c_void);
    data.dwStateAction = WTD_STATEACTION_VERIFY;
    let status = unsafe { WinVerifyTrust(hwnd, &mut action, (data as *mut WINTRUST_DATA).cast()) };
    let name = if status == 0 { unsafe { signer_name(data.hWVTStateData) } } else { None };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe { WinVerifyTrust(hwnd, &mut action, (data as *mut WINTRUST_DATA).cast()) };
    (status, name)
}

/// 从校验状态里取第一个签名者的叶证书显示名。
unsafe fn signer_name(state: HANDLE) -> Option<String> {
    let prov = unsafe { WTHelperProvDataFromStateData(state) };
    if prov.is_null() {
        return None;
    }
    let signer = unsafe { WTHelperGetProvSignerFromChain(prov, 0, false, 0) };
    if signer.is_null() {
        return None;
    }
    let cert = unsafe { WTHelperGetProvCertFromChain(signer, 0) };
    if cert.is_null() {
        return None;
    }
    let ctx = unsafe { (*cert).pCert };
    if ctx.is_null() {
        return None;
    }
    let mut buf = [0u16; 256];
    let n = unsafe { CertGetNameStringW(ctx, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None, Some(&mut buf)) } as usize;
    // 返回值含结尾的 NUL。
    (n > 1).then(|| String::from_utf16_lossy(&buf[..n - 1]))
}

/// 目录签名：算文件哈希，找到登记它的 catalog，再让 `WinVerifyTrust` 按 catalog 校验。
/// 先试 SHA-256，再试系统默认（SHA-1）：旧的 catalog 用 SHA-1 登记。
fn catalog_signature(file: HANDLE, wpath: &[u16]) -> Signature {
    for alg in [w!("SHA256"), PCWSTR::null()] {
        match catalog_with(alg, file, wpath) {
            Some(sig) => return sig,
            None => continue,
        }
    }
    Signature::Unsigned
}

/// 用指定哈希算法查 catalog；没有 catalog 登记该哈希时返回 `None`。
fn catalog_with(alg: PCWSTR, file: HANDLE, wpath: &[u16]) -> Option<Signature> {
    unsafe {
        let mut admin = 0isize;
        CryptCATAdminAcquireContext2(&mut admin, None, alg, None, None).ok()?;
        let result = (|| {
            let mut len = 0u32;
            CryptCATAdminCalcHashFromFileHandle2(admin, file, &mut len, None, None).ok()?;
            let mut hash = vec![0u8; len as usize];
            CryptCATAdminCalcHashFromFileHandle2(admin, file, &mut len, Some(hash.as_mut_ptr()), None).ok()?;
            hash.truncate(len as usize);
            let cat = CryptCATAdminEnumCatalogFromHash(admin, &hash, None, None);
            if cat == 0 {
                return None;
            }
            let mut ci = CATALOG_INFO { cbStruct: size_of::<CATALOG_INFO>() as u32, ..Default::default() };
            let got = CryptCATCatalogInfoFromContext(cat, &mut ci, 0);
            let sig = got.ok().map(|()| {
                let tag: String = hash.iter().map(|b| format!("{b:02X}")).collect();
                let tag = wide0(&tag);
                let mut cinfo = WINTRUST_CATALOG_INFO {
                    cbStruct: size_of::<WINTRUST_CATALOG_INFO>() as u32,
                    pcwszCatalogFilePath: PCWSTR(ci.wszCatalogFile.as_ptr()),
                    pcwszMemberTag: PCWSTR(tag.as_ptr()),
                    pcwszMemberFilePath: PCWSTR(wpath.as_ptr()),
                    hMemberFile: file,
                    pbCalculatedFileHash: hash.as_mut_ptr(),
                    cbCalculatedFileHash: hash.len() as u32,
                    hCatAdmin: admin,
                    ..Default::default()
                };
                let mut data = new_data(WTD_CHOICE_CATALOG);
                data.Anonymous.pCatalog = &mut cinfo;
                let (status, name) = run(&mut data);
                if status == 0 { Signature::Signed(name.unwrap_or_default()) } else { Signature::Invalid }
            });
            let _ = CryptCATAdminReleaseCatalogContext(admin, cat, 0);
            Some(sig.unwrap_or(Signature::Invalid))
        })();
        let _ = CryptCATAdminReleaseContext(admin, 0);
        result
    }
}

/// 把当前线程降到最低优先级（后台校验线程用，避免与采样和界面抢 CPU）。
pub fn lower_current_thread_priority() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_LOWEST};
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn system32(name: &str) -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32").join(name)
    }

    #[test]
    fn system_executable_is_signed_by_microsoft() {
        // svchost.exe 通常靠 catalog 签名；explorer.exe 在 Windows 目录下。
        let sig = verify(&system32("svchost.exe"));
        match sig {
            Signature::Signed(name) => assert!(name.contains("Microsoft"), "签名者 {name}"),
            other => panic!("svchost.exe 应已验证：{other:?}"),
        }
    }

    #[test]
    fn plain_text_file_is_unsigned() {
        let p = std::env::temp_dir().join("tml-unsigned-test.exe");
        std::fs::write(&p, b"MZ not a real exe").unwrap();
        let sig = verify(&p);
        let _ = std::fs::remove_file(&p);
        assert!(matches!(sig, Signature::Unsigned | Signature::Invalid), "{sig:?}");
    }

    #[test]
    fn missing_file_is_invalid() {
        assert_eq!(verify(Path::new(r"C:\no\such\file.exe")), Signature::Invalid);
    }

    #[test]
    fn slot_is_set_once_and_compared_by_identity() {
        let a = SigSlot::default();
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(a, SigSlot::default());
        assert!(a.get().is_none());
        a.set(Signature::Unsigned);
        a.set(Signature::Invalid);
        assert_eq!(b.get(), Some(&Signature::Unsigned));
    }

    #[test]
    fn rank_orders_unsigned_invalid_signed() {
        assert!(Signature::Unsigned.rank() < Signature::Invalid.rank());
        assert!(Signature::Invalid.rank() < Signature::Signed(String::new()).rank());
    }
}

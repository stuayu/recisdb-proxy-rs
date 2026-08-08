//! PC/SC カードリーダーの列挙。
//!
//! libaribb25 は「見つかったリーダーに片っ端から繋ぐ」しかできず、どのリーダーが
//! あるのかを呼び出し側へ教えてくれない。B-CAS 以外のリーダー (銀行カード用の
//! EMV リーダー等) が挿さっている環境では、利用者に選ばせる以外に正解を決める
//! 方法がないため、ここで `SCardListReaders` を直接叩いて一覧を返す。
//!
//! # 型について (重要)
//!
//! PC/SC の型はプラットフォームごとに幅が違う。ここを間違えると、出力引数の
//! 上位ビットに未初期化のスタックが残って巨大な長さが返り、その値を使った
//! ポインタ計算でクラッシュする。実際 libaribb25 本体がこれで macOS で
//! SIGSEGV していた (`build.rs` のパッチ参照)。
//!
//! - macOS (PCSC.framework): `DWORD = uint32_t`, `SCARDCONTEXT = int32_t`
//! - Linux (pcsclite): `DWORD = unsigned long`, `SCARDCONTEXT = long`
//! - Windows (winscard): `DWORD = u32`, `SCARDCONTEXT = ULONG_PTR`
//!
//! Linux では PC/SC をリンクせず `pcsc_shim` が dlopen したものに解決される
//! (`b25-sys/src/pcsc_shim.rs` 参照)。したがってこのモジュールの `extern` 宣言は
//! Linux でもそのまま shim 経由で動く。

use std::ffi::CStr;

#[cfg(target_os = "macos")]
mod ffi_types {
    pub type Dword = u32;
    pub type Long = i32;
    pub type ScardContext = i32;
}

#[cfg(all(unix, not(target_os = "macos")))]
mod ffi_types {
    pub type Dword = std::os::raw::c_ulong;
    pub type Long = std::os::raw::c_long;
    pub type ScardContext = std::os::raw::c_long;
}

#[cfg(windows)]
mod ffi_types {
    pub type Dword = u32;
    pub type Long = i32;
    pub type ScardContext = usize;
}

use ffi_types::{Dword, Long, ScardContext};

const SCARD_S_SUCCESS: Long = 0;
const SCARD_SCOPE_USER: Dword = 0;

extern "system" {
    fn SCardEstablishContext(
        dw_scope: Dword,
        reserved1: *const std::ffi::c_void,
        reserved2: *const std::ffi::c_void,
        context: *mut ScardContext,
    ) -> Long;
    fn SCardReleaseContext(context: ScardContext) -> Long;
    // Windows の winscard.h では `SCardListReaders` はマクロで、実体は
    // `SCardListReadersA` / `SCardListReadersW` しか export されていない。
    // プレーン名で宣言すると unresolved external になるので ANSI 版へ張る
    // (返る名前は表示用なので ANSI で十分)。
    #[cfg_attr(windows, link_name = "SCardListReadersA")]
    fn SCardListReaders(
        context: ScardContext,
        groups: *const u8,
        readers: *mut u8,
        readers_len: *mut Dword,
    ) -> Long;
}

/// 接続されている PC/SC カードリーダーの名前を返す。
///
/// PC/SC デーモンが動いていない・リーダーが1つも無い場合は空の `Vec` を返す
/// (エラーにはしない。「まだ挿していない」は正常な状態であり、UI 側は
/// 「見つかりません」と出せばよいため)。
pub fn list_card_readers() -> Vec<String> {
    unsafe { list_card_readers_inner() }
}

unsafe fn list_card_readers_inner() -> Vec<String> {
    let mut context: ScardContext = 0;
    if SCardEstablishContext(
        SCARD_SCOPE_USER,
        std::ptr::null(),
        std::ptr::null(),
        &mut context,
    ) != SCARD_S_SUCCESS
    {
        return Vec::new();
    }

    let readers = list_with_context(context);
    SCardReleaseContext(context);
    readers
}

unsafe fn list_with_context(context: ScardContext) -> Vec<String> {
    // 1回目は長さの問い合わせ。`len` は必ず 0 で初期化しておく — 失敗時に
    // 未初期化の値を使わないため。
    let mut len: Dword = 0;
    if SCardListReaders(context, std::ptr::null(), std::ptr::null_mut(), &mut len)
        != SCARD_S_SUCCESS
    {
        return Vec::new();
    }
    if len == 0 {
        return Vec::new();
    }

    let mut buf = vec![0u8; len as usize];
    if SCardListReaders(context, std::ptr::null(), buf.as_mut_ptr(), &mut len) != SCARD_S_SUCCESS {
        return Vec::new();
    }
    // 2回目の呼び出しが実際に書いた長さまでで切り詰める (要求より短いことがある)。
    buf.truncate((len as usize).min(buf.len()));

    parse_multi_string(&buf)
}

/// PC/SC の「マルチストリング」(NUL 区切り + 末尾に空文字列) を分解する。
///
/// 末尾の終端が欠けていても最後の要素を落とさない。UTF-8 として不正な名前は
/// 捨てずに lossy 変換する — 表示して選ばせるのが目的で、化けた名前でも
/// 「そういうリーダーがある」ことは伝わるため。
fn parse_multi_string(buf: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = buf;
    while let Some(pos) = rest.iter().position(|&b| b == 0) {
        if pos == 0 {
            break; // 空文字列 = 終端
        }
        if let Ok(s) = CStr::from_bytes_with_nul(&rest[..=pos]) {
            out.push(s.to_string_lossy().into_owned());
        }
        rest = &rest[pos + 1..];
    }
    // 終端 NUL が無いまま尽きた場合の取りこぼしを拾う。
    if !rest.is_empty() && !rest.iter().all(|&b| b == 0) {
        out.push(String::from_utf8_lossy(rest).into_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::parse_multi_string;

    #[test]
    fn parses_a_normal_multi_string() {
        let buf = b"Reader A\0Reader B\0\0";
        assert_eq!(parse_multi_string(buf), vec!["Reader A", "Reader B"]);
    }

    #[test]
    fn stops_at_the_empty_terminator_and_ignores_trailing_padding() {
        let buf = b"Only One\0\0\0\0\0";
        assert_eq!(parse_multi_string(buf), vec!["Only One"]);
    }

    #[test]
    fn recovers_a_final_entry_that_lost_its_terminator() {
        let buf = b"Reader A\0Reader B";
        assert_eq!(parse_multi_string(buf), vec!["Reader A", "Reader B"]);
    }

    #[test]
    fn empty_input_yields_no_readers() {
        assert!(parse_multi_string(b"").is_empty());
        assert!(parse_multi_string(b"\0").is_empty());
    }

    #[test]
    fn invalid_utf8_is_kept_rather_than_dropped() {
        // 名前が化けていても「リーダーが存在する」ことは伝える必要がある。
        let buf = b"Bad\xffName\0\0";
        assert_eq!(parse_multi_string(buf).len(), 1);
    }
}

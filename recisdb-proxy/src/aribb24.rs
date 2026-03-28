// recisdb-proxy/src/aribb24.rs

/// vendor/aribb24 を全プラットフォームで静的リンクして ARIB STD-B24 を UTF-8 にデコードする。
/// build.rs が aribb24_wrap.c + vendor/aribb24/src/*.c をコンパイルする。
extern "C" {
    fn C_AribB24DecodeToUtf8(
        in_ptr: *const u8,
        in_len: usize,
        out_ptr: *mut i8,
        out_len: usize,
    ) -> usize;
}

/// ARIB STD-B24 バイト列を UTF-8 文字列にデコードする。
pub fn decode_arib_b24(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    // 出力が増えることがあるので余裕を持たせる
    let mut out = vec![0u8; bytes.len() * 8 + 32];

    let written = unsafe {
        C_AribB24DecodeToUtf8(
            bytes.as_ptr(),
            bytes.len(),
            out.as_mut_ptr() as *mut i8,
            out.len(),
        )
    };

    out.truncate(written);
    String::from_utf8_lossy(&out).to_string()
}

/// 既存コード互換：decode_arib_string() を呼んでいる箇所があるので同名を提供
pub fn decode_arib_string(bytes: &[u8]) -> String {
    decode_arib_b24(bytes)
}

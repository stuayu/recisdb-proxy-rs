
// recisdb-proxy/src/aribb24.rs

/// Windows では aribb24 (Cラッパ) を呼び出して ARIB STD-B24 を UTF-8 にデコードする。
#[cfg(target_os = "windows")]
mod imp {
    extern "C" {
        fn C_AribB24DecodeToUtf8(
            in_ptr: *const u8,
            in_len: usize,
            out_ptr: *mut i8,
            out_len: usize,
        ) -> usize;
    }

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
}

#[cfg(not(target_os = "windows"))]
mod imp {
    // 非Windows用の暫定（必要なら後で改善）
    pub fn decode_arib_b24(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).to_string()
    }
}

/// 本命：ARIB STD-B24 -> UTF-8
pub use imp::decode_arib_b24;

/// 既存コード互換：decode_arib_string() を呼んでいる箇所があるので同名を提供
pub fn decode_arib_string(bytes: &[u8]) -> String {
    decode_arib_b24(bytes)
}

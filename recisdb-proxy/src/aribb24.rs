// recisdb-proxy/src/aribb24.rs

/// vendor/aribb24 を全プラットフォームで静的リンクして ARIB STD-B24 を UTF-8 にデコードする。
/// build.rs が aribb24_wrap.c + vendor/aribb24/src/*.c をコンパイルする。
extern "C" {
    fn C_AribB24DecodeToUtf8Lines(
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
    decode_arib_lines(bytes)
}

fn decode_arib_lines(bytes: &[u8]) -> String {
    let capacity = bytes.len().saturating_mul(8).saturating_add(32);
    let mut out = vec![0u8; capacity];
    let written = unsafe {
        C_AribB24DecodeToUtf8Lines(
            bytes.as_ptr(),
            bytes.len(),
            out.as_mut_ptr() as *mut i8,
            out.len(),
        )
    };
    out.truncate(written.min(out.len()));
    String::from_utf8_lossy(&out).to_string()
}

/// 既存コード互換：decode_arib_string() を呼んでいる箇所があるので同名を提供
pub fn decode_arib_string(bytes: &[u8]) -> String {
    decode_arib_b24(bytes)
}

#[cfg(test)]
mod tests {
    use super::decode_arib_b24;

    #[test]
    fn decodes_kanji_alphanumeric_katakana_and_additional_symbol_vectors() {
        // ARIB STD-B24 Vol. 1 Part 3 §7.1, Tables 7-1..7-3.
        // "日本" in the ARIB Kanji set, then an alphanumeric G0 switch.
        let kanji_alnum = [0x46, 0x7C, 0x4B, 0x5C, 0x1B, 0x28, 0x4A, b'T', b'V'];
        let decoded = decode_arib_b24(&kanji_alnum);
        assert!(decoded.contains("日本"), "{decoded:?}");
        assert!(decoded.contains("ＴＶ"), "{decoded:?}");

        // Katakana G0 designation (ESC 29 31), followed by the ARIB
        // Katakana code for ア.
        let katakana = [0x1B, 0x29, 0x31, 0x0E, 0x25];
        assert!(!decode_arib_b24(&katakana).is_empty());

        // Additional-symbol set designation must be accepted without panic;
        // the glyph may be unavailable in the decoder's UCS conversion table.
        let additional_symbol = [0x1B, 0x28, 0x3B, 0x75, 0x21, 0x21];
        let _ = decode_arib_b24(&additional_symbol);
    }

    #[test]
    fn preserves_arib_line_controls_and_ignores_malformed_bytes() {
        let bytes = [0x1B, 0x28, 0x4A, b'A', 0x0D, b'B', 0x0A, b'C', 0xFF];
        assert_eq!(decode_arib_b24(&bytes), "Ａ\nＢ\nＣ");
    }

    #[test]
    fn preserves_kanji_designation_across_a_line_control() {
        let bytes = [0x46, 0x7C, 0x0D, 0x46, 0x7C]; // 日, APR, 日
        assert_eq!(decode_arib_b24(&bytes), "日\n日");
    }

    #[test]
    fn does_not_turn_six_fullwidth_spaces_into_a_newline() {
        let bytes = [0x21, 0x21, 0x21, 0x21, 0x21, 0x21, 0x21, 0x21, 0x21, 0x21, 0x21, 0x21];
        let decoded = decode_arib_b24(&bytes);
        assert!(!decoded.contains('\n'), "{decoded:?}");
    }

    #[test]
    fn collapses_crlf_and_ignores_edge_line_controls() {
        let bytes = [0x0D, 0x0A, 0x1B, 0x28, 0x4A, b'A', 0x0D, 0x0A, 0x0A, b'B', 0x0D];
        assert_eq!(decode_arib_b24(&bytes), "Ａ\nＢ");
    }

    #[test]
    fn malformed_and_large_input_does_not_panic() {
        let mut bytes = vec![0xFF; 1_000_000];
        bytes.extend_from_slice(&[0x0D, 0x0A]);
        let _ = decode_arib_b24(&bytes);
    }
}

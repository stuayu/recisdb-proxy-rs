//! カードリーダーが実際に挿さっている環境でだけ意味のあるテスト。
//!
//! すべて `#[ignore]`。CI には PC/SC デーモンもリーダーも無いため。
//!
//! ```bash
//! cargo test -p b25-sys --test card_reader_hardware --features prioritized_card_reader -- --ignored --nocapture
//! ```
//!
//! `init_decoder` と `set_name` は、どちらも 2026-08 に macOS で
//! クラッシュしていた経路の回帰テスト:
//!
//! - `init_decoder`: libaribb25 が `SCardListReaders` に 64bit の
//!   `unsigned long` を渡していたため、macOS (`DWORD = uint32_t`) では長さの
//!   上位32bitが未初期化のまま残り、SIGSEGV していた。
//! - `set_name`: `override_card_reader_name_pattern` が `static const` の
//!   バッファへ `_tcscpy` していたため、読み取り専用セクションへの書き込みで
//!   SIGBUS していた。
//!
//! どちらも `b25-sys/build.rs` のビルド時パッチで修正済み。落ちたら
//! パッチが当たっていない (upstream の構造が変わった) ことを疑う。

#[test]
#[ignore]
fn init_decoder_does_not_crash_while_probing_readers() {
    let decoder = b25_sys::StreamDecoder::new(b25_sys::DecoderOptions::default());
    println!("StreamDecoder::new -> {:?}", decoder.as_ref().err());
    assert!(decoder.is_ok());
}

#[cfg(feature = "prioritized_card_reader")]
#[test]
#[ignore]
fn set_card_reader_name_does_not_crash() {
    let readers = b25_sys::list_card_readers();
    println!("readers: {readers:?}");
    let Some(name) = readers.first() else {
        println!("カードリーダーが無いのでスキップ");
        return;
    };
    assert!(b25_sys::set_card_reader_name(name));
}

#[test]
#[ignore]
fn list_card_readers_reports_what_pcsc_sees() {
    println!("{:?}", b25_sys::list_card_readers());
}

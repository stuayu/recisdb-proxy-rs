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

/// recisdb-proxy は1プロセスでチューナーごとにデコーダーを持つ。SoftCAS 系の
/// `winscard.dll` / `libpcsckai.so` は毎回同じコンテキスト値を返すため、2つ目以降の
/// デコーダーが「カードリーダーなし」になっていた(2026-11, eefd4f0)。
///
/// `B25_TEST_TS` にスクランブルされた TS を指定すると、全デコーダーで復号し、
/// 出力にスクランブル済みパケットが残らないことも確かめる。
#[test]
#[ignore]
fn concurrent_decoders_all_initialize_and_descramble() {
    use std::io::{Read, Write};

    let input = std::env::var("B25_TEST_TS")
        .ok()
        .map(|path| std::fs::read(path).expect("B25_TEST_TS を読めない"));
    let mut decoders = Vec::new();
    for index in 0..4 {
        let decoder = b25_sys::StreamDecoder::new(b25_sys::DecoderOptions::default());
        assert!(
            decoder.is_ok(),
            "decoder #{index} failed while {} others are alive: {:?}",
            decoders.len(),
            decoder.err()
        );
        decoders.push(decoder.unwrap());
    }
    let Some(input) = input else {
        return;
    };
    for (index, decoder) in decoders.iter_mut().enumerate() {
        let mut output = Vec::new();
        let mut buffer = vec![0u8; 1 << 20];
        for chunk in input.chunks(188 * 256) {
            decoder.write_all(chunk).unwrap();
            loop {
                let length = decoder.read(&mut buffer).unwrap();
                if length == 0 {
                    break;
                }
                output.extend_from_slice(&buffer[..length]);
            }
        }
        decoder.flush().unwrap();
        loop {
            let length = decoder.read(&mut buffer).unwrap();
            if length == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..length]);
        }
        let packets = output.chunks_exact(188).filter(|p| p[0] == 0x47);
        let (total, scrambled) = packets.fold((0usize, 0usize), |(t, s), p| {
            (t + 1, s + usize::from(p[3] & 0xc0 != 0))
        });
        println!("decoder #{index}: {total} packets, {scrambled} scrambled");
        assert!(total > 0, "decoder #{index} produced no packets");
        // ECM 到着前の先頭パケットだけはスクランブルのまま出ることがある。
        assert!(
            scrambled * 100 <= total,
            "decoder #{index} left {scrambled}/{total} packets scrambled"
        );
    }
}

/// recisdb-proxy はデコーダーを `b25-init` スレッドで作り、SetChannel と並行させた
/// あとで読み取りスレッドへ渡す(`tuner/shared.rs` の `SendB25`)。作成と使用が別
/// スレッドでも PC/SC ハンドルが使えることを、実カードで確かめる。
#[test]
#[ignore]
fn decoder_created_on_another_thread_descrambles() {
    use std::io::{Read, Write};

    struct Moved(b25_sys::StreamDecoder);
    // SAFETY: 作成スレッドは join 後に終了しており、同時に触れるスレッドはない。
    unsafe impl Send for Moved {}

    let decoder = std::thread::spawn(|| {
        Moved(b25_sys::StreamDecoder::new(b25_sys::DecoderOptions::default()).unwrap())
    })
    .join()
    .expect("init thread panicked");
    let mut decoder = decoder.0;
    let Ok(path) = std::env::var("B25_TEST_TS") else {
        return;
    };
    let input = std::fs::read(path).expect("B25_TEST_TS を読めない");
    let mut output = Vec::new();
    let mut buffer = vec![0u8; 1 << 20];
    for chunk in input.chunks(188 * 256) {
        decoder.write_all(chunk).unwrap();
        loop {
            let length = decoder.read(&mut buffer).unwrap();
            if length == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..length]);
        }
    }
    decoder.flush().unwrap();
    loop {
        let length = decoder.read(&mut buffer).unwrap();
        if length == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..length]);
    }
    let packets = output.chunks_exact(188).filter(|p| p[0] == 0x47);
    let (total, scrambled) = packets.fold((0usize, 0usize), |(t, s), p| {
        (t + 1, s + usize::from(p[3] & 0xc0 != 0))
    });
    println!("cross-thread decoder: {total} packets, {scrambled} scrambled");
    assert!(total > 0);
    assert!(scrambled * 100 <= total, "{scrambled}/{total} scrambled");
}

#[test]
#[ignore]
fn list_card_readers_reports_what_pcsc_sees() {
    println!("{:?}", b25_sys::list_card_readers());
}

#[cfg(all(target_os = "macos", feature = "prioritized_card_reader"))]
#[test]
#[ignore = "requires a running DriverHost_PX4 and an inserted B-CAS card"]
fn px4_driverhost_internal_reader_initializes_b25() {
    let readers = b25_sys::list_card_readers();
    let Some(reader) = readers
        .iter()
        .find(|reader| reader.starts_with("PX4 DriverHost: "))
    else {
        panic!("PX4 DriverHost card reader is not visible to B25: {readers:?}");
    };

    let decoder =
        b25_sys::StreamDecoder::with_card_reader(b25_sys::DecoderOptions::default(), Some(reader));
    assert!(
        decoder.is_ok(),
        "B25 failed to initialize from PX4 DriverHost card reader {reader:?}: {:?}",
        decoder.err()
    );
}

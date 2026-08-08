//! プレビュー自動セットアップの実機テスト。
//!
//! ネットワークアクセス・外部プロセス起動・(Unix では) tsreadex のビルドを
//! 伴うため `#[ignore]`。
//!
//! ```bash
//! cargo test -p recisdb-proxy --test preview_autosetup_hardware -- --ignored --nocapture
//! ```

use std::path::PathBuf;

#[test]
#[ignore]
fn ensure_preview_ready_sets_everything_up() {
    let tmp = std::env::temp_dir().join(format!("recisdb-preview-setup-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();

    // 設定ファイルの [preview] を書き戻せるか(= 次回起動で巻き戻らないか)も見る。
    let config_path = tmp.join("recisdb-proxy.toml");
    std::fs::write(
        &config_path,
        "[server]\nlisten = \"0.0.0.0:40070\"\n\n[preview]\ncommand_path = \"\"\npreprocessor_path = \"\"\n",
    )
    .unwrap();

    let db = recisdb_proxy::database::Database::open_in_memory().unwrap();
    let report =
        recisdb_proxy::preview_setup::ensure_preview_ready(&db, &tmp, Some(&config_path)).unwrap();

    println!("encoder      : {} ({})", report.encoder_path, report.encoder_source);
    println!("video encoder: {}", report.video_encoder);
    println!("preprocessor : {}", report.preprocessor_path);
    for w in &report.warnings {
        println!("warning      : {w}");
    }

    assert!(report.enabled);
    assert!(PathBuf::from(&report.encoder_path).exists(), "エンコーダの実体が無い");

    // DB に書かれ、かつ有効になっていること。
    let (enabled, command_path, preprocessor_path, _args, _timeout) =
        db.get_preview_encoder_config().unwrap();
    assert!(enabled);
    assert_eq!(command_path, report.encoder_path);
    assert_eq!(preprocessor_path, report.preprocessor_path);

    // プロファイルが ffmpeg 方言になっていること (QSVEncC のままだと動かない)。
    let profile = db
        .get_encode_profile_by_purpose("preview")
        .unwrap()
        .expect("preview プロファイルが無い");
    let args = profile.extra_args.unwrap_or_default();
    assert!(args.contains("-f mpegts -i pipe:0"), "ffmpeg 用の引数になっていない: {args}");
    assert!(args.contains(&report.video_encoder), "選ばれたエンコーダが引数に入っていない");

    // TOML にも書き戻されていること。ここが抜けると起動時に DB が上書きされる。
    let toml = std::fs::read_to_string(&config_path).unwrap();
    assert!(toml.contains(&report.encoder_path), "TOML にエンコーダのパスが無い:\n{toml}");

    std::fs::remove_dir_all(&tmp).ok();
}

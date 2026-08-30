//! Encode profile CRUD (STREAMING_DESIGN.md §5.3, §9 P5).
//!
//! `encode_profiles` replaces the old single-row `tsreplace_config` mindset
//! with a small catalogue: each row supplies codec/bitrate/extra-argument
//! choices for a `purpose` ('record' / 'preview' / 'view'). `command_path`
//! deliberately stays out of this table and out of every request type in
//! this module — it remains governed solely by `tsreplace_config.command_path`
//! (TOML-only, REVIEW S1); see `Database::set_tsreplace_command_path`.

use super::{Database, EncodeProfileRecord, Result};
use rusqlite::{params, OptionalExtension};

/// HEVC hardware decoders `preview_setup` may pick for the BS4K template.
/// Kept here (not in `preview_setup`) because
/// [`preview_4k_extra_args_is_auto_generated`] has to recognize every
/// template this codebase can generate, and that includes one per decoder.
pub(crate) const KNOWN_HEVC_HW_DECODERS: &[&str] = &[
    "hevc_qsv",
    "hevc_cuvid",
    "hevc_vaapi",
    "hevc_videotoolbox",
];

const KNOWN_PREVIEW_ENCODERS: &[&str] = &[
    "libx264",
    "h264_videotoolbox",
    "h264_qsv",
    "h264_nvenc",
    "h264_amf",
    "h264_vaapi",
];

/// Recommended encoder arguments for the seeded `preview-h264` profile.
///
/// A direct QSVEncC invocation (the `[preview] command_path` executable runs
/// these as-is), designed to sit behind the recommended tsreadex stage-1
/// (`DEFAULT_PREVIEW_PREPROCESSOR_ARGUMENTS` in `database::mod`): tsreadex
/// selects the service via `-n {SID}` and converts captions to ID3
/// timed-metadata, so no `{SID}`/`--service` appears here and
/// `--data-copy timed_id3` carries the caption metadata through to
/// aribb24.js. H.264 ~2Mbps VBR, deinterlaced, AAC stereo — the
/// KonomiTV-equivalent browser-preview baseline.
pub(crate) const DEFAULT_PREVIEW_ENCODE_ARGS: &str =
    "--avhw -i - --input-format mpegts --input-analyze 0.6 --input-probesize 1000K \
     --interlace tff --vpp-deinterlace normal -c h264 --profile high \
     --vbr 2000 --max-bitrate 3000 --gop-len 60 --dar 16:9 \
     --audio-codec aac --audio-bitrate 192 --audio-samplerate 48000 \
     --data-copy timed_id3 --output-format mpegts -o -";

/// Broken BS4K preview arguments seeded before the ffmpeg pipeline was used.
///
/// This is retained only as a migration sentinel. It is rigaya syntax and
/// cannot be passed to the ffmpeg executable selected by `preview_setup`.
pub(crate) const DEFAULT_PREVIEW_4K_ENCODE_ARGS: &str =
    "--avhw -i - --input-format mpegts --input-analyze 0.6 --input-probesize 1000K \
     --vpp-resize algo=auto --output-res 1920x1080 -c h264 --profile high \
     --vbr 4000 --max-bitrate 6000 --gop-len 60 --dar 16:9 \
     --audio-codec aac --audio-bitrate 192 --audio-samplerate 48000 \
     --data-copy timed_id3 --output-format mpegts -o -";

/// The template seeded before the two-stage `[preview]` pipeline existed:
/// QSVEncC wrapped inside a tsreplace command line. Kept only so
/// `seed_default_encode_profiles` can recognize an untouched legacy row and
/// migrate it to `DEFAULT_PREVIEW_ENCODE_ARGS`.
const LEGACY_PREVIEW_ENCODE_ARGS: &str =
    "-i - -o - --preserve-other-services -e QSVEncC64.exe -i - \
     --input-format mpegts --tff --vpp-deinterlace normal \
     -c h264 --vbr 2000 --max-bitrate 3000 --gop-len 60 \
     --output-format mpegts -o -";

/// ffmpeg 用のプレビュー引数を組み立てる。
///
/// `DEFAULT_PREVIEW_ENCODE_ARGS` は rigaya 系 (QSVEncC 等) の方言で、ffmpeg では
/// 一切通らない。自動セットアップ (`preview_setup`) は ffmpeg を使うため、
/// そちらで選ばれた映像エンコーダ名を埋め込んだ引数をここで作る。
///
/// stdin から mpegts を読み stdout へ mpegts を書く。前段 tsreadex
/// (`DEFAULT_PREVIEW_PREPROCESSOR_ARGUMENTS` の `-d 13`) が字幕を ID3
/// timed-metadata に変換して data ストリームに載せるので、`-map 0:d?` +
/// `-c:d copy` でそれを通す (aribb24.js が受け取る)。`?` はデータストリームが
/// 無い放送でも失敗させないため。
///
/// 引数は `encoder_pool` が `split_whitespace` で分割する (シェル解釈なし) ので、
/// **1トークンの中に空白を入れてはいけない**。
pub fn preview_encode_args_ffmpeg(video_encoder: &str) -> String {
    format!(
        "-hide_banner -loglevel error -fflags +discardcorrupt+genpts \
         -analyzeduration 600000 -probesize 1000000 \
         -f mpegts -i pipe:0 \
         -map 0:v:0 -map 0:a:0 -map 0:d? -copy_unknown \
         -vf yadif=0:-1:1 \
         -c:v {video_encoder} {tuning} -b:v 2000k -maxrate 3000k -bufsize 4000k \
         -g 60 -aspect 16:9 \
         -c:a aac -b:a 192k -ar 48000 -ac 2 \
         -af aresample=async=1:min_hard_comp=0.100000:first_pts=0 \
         -c:d copy \
         -f mpegts -flush_packets 1 -muxdelay 0 -muxpreload 0 pipe:1",
        tuning = video_encoder_tuning(video_encoder)
    )
}

/// Superseded ffmpeg BS4K templates, kept only as migration sentinels so a
/// row still holding one is recognized as this codebase's own output and
/// upgraded. See [`preview_4k_encode_args_ffmpeg`] for the measurements that
/// retired each of them.
fn legacy_preview_4k_encode_args_ffmpeg(video_encoder: &str) -> [String; 2] {
    let tuning = video_encoder_tuning(video_encoder);
    [
        // 1st: plain software decode. 0.27x realtime, 17.3s to first byte.
        format!(
            "-hide_banner -loglevel error -fflags +discardcorrupt+genpts \
             -analyzeduration 600000 -probesize 1000000 -f mpegts -i pipe:0 \
             -map 0:v:0 -map 0:a:0 -map 0:d? -copy_unknown \
             -vf scale=1920:1080 \
             -c:v {video_encoder} {tuning} -b:v 4000k -maxrate 6000k -bufsize 8000k \
             -g 60 -aspect 16:9 -c:a aac -b:a 192k -ar 48000 -ac 2 \
             -af aresample=async=1:min_hard_comp=0.100000:first_pts=0 -c:d copy \
             -f mpegts -flush_packets 1 -muxdelay 0 -muxpreload 0 pipe:1"
        ),
        // 2nd: throttled decode but still 1080p. 0.99x over 40s, but only
        // 0.88x sustained over 120s, so it still drifts behind.
        format!(
            "-hide_banner -loglevel error -fflags +discardcorrupt+genpts \
             -analyzeduration 600000 -probesize 1000000 \
             -skip_loop_filter:v all -skip_frame:v noref \
             -f mpegts -i pipe:0 \
             -map 0:v:0 -map 0:a:0 -map 0:d? -copy_unknown \
             -vf scale=1920:1080:flags=fast_bilinear \
             -c:v {video_encoder} {tuning} -b:v 4000k -maxrate 6000k -bufsize 8000k \
             -g 60 -aspect 16:9 -c:a aac -b:a 192k -ar 48000 -ac 2 \
             -af aresample=async=1:min_hard_comp=0.100000:first_pts=0 -c:d copy \
             -f mpegts -flush_packets 1 -muxdelay 0 -muxpreload 0 pipe:1"
        ),
    ]
}

/// BS4K preview arguments for a machine whose ffmpeg can decode HEVC in
/// hardware.
///
/// This is the good case: the 2160/59.94p Main10 source is decoded on the
/// GPU, so none of the decode-side throttles in
/// [`preview_4k_encode_args_ffmpeg`] are needed and the preview keeps full
/// 1080p at the source frame rate. `preview_setup` only selects a decoder
/// here after actually decoding a sample HEVC clip with it, because being
/// listed in `-decoders` says nothing about whether a GPU backs it.
pub fn preview_4k_encode_args_ffmpeg_hwdec(video_encoder: &str, hevc_decoder: &str) -> String {
    format!(
        "-hide_banner -loglevel error -fflags +discardcorrupt+genpts \
         -analyzeduration 600000 -probesize 1000000 \
         -c:v {hevc_decoder} \
         -f mpegts -i pipe:0 \
         -map 0:v:0 -map 0:a:0 -map 0:d? -copy_unknown \
         -vf scale=1920:1080 \
         -c:v {video_encoder} {tuning} -b:v 4000k -maxrate 6000k -bufsize 8000k \
         -g 60 -aspect 16:9 -c:a aac -b:a 192k -ar 48000 -ac 2 \
         -af aresample=async=1:min_hard_comp=0.100000:first_pts=0 -c:d copy \
         -f mpegts -flush_packets 1 -muxdelay 0 -muxpreload 0 pipe:1",
        tuning = video_encoder_tuning(video_encoder)
    )
}

/// Build ffmpeg arguments for BS4K preview **when no hardware HEVC decoder
/// is available**. Use [`preview_4k_encode_args_ffmpeg_hwdec`] when there is
/// one; `preview_setup` decides which by probing.
///
/// The tsreadex input and stream mapping match the normal preview, but the
/// source is progressive 2160/59.94p HEVC Main10, so there is no deinterlacer
/// and the picture is scaled to 1920x1080.
///
/// **The decoder side is what costs, not the encoder.** Measured on the
/// verification machine (Intel iGPU, `h264_qsv` available):
///
/// | template | realtime (40s) | realtime (120s) | first byte |
/// |---|---|---|---|
/// | 1080p, plain software decode | 0.27x | — | 17.3s |
/// | 1080p, `-skip_loop_filter:v all` only | 0.66x | — | 2.1s |
/// | 1080p, + `-skip_frame:v noref` | 0.99x | 0.88x | 1.6s |
/// | **720p, + `-skip_frame:v noref`** (this one) | 0.99x | **0.99x** | 2.3s |
///
/// 1080p looks fine over a short sample and then drifts ~12% behind over two
/// minutes, which is exactly the "breaks up after a while" complaint. 720p is
/// the resolution this machine actually sustains. An administrator with a
/// faster decoder can raise it; the seed leaves an edited row alone.
///
/// Hardware HEVC decode is not an option to fall back on: `hevc_qsv` reports
/// `Error decoding stream header: unsupported (-3)`, `-init_hw_device qsv=hw`
/// fails with `Error creating a MFX session: -9`, and `-hwaccel d3d11va`
/// silently falls back to the software decoder. Only the **encoder** side of
/// QSV works there.
///
/// So the decode work itself is cut down: deblocking/SAO is skipped and
/// non-reference frames are dropped, which halves the preview to ~30fps
/// (1151 frames in 39.7s measured). That is a deliberate trade — a browser
/// preview that keeps up at 30fps beats one that falls behind at 60.
/// Both options are scoped to `:v` because an unscoped `-skip_frame` is also
/// applied to the AAC decoder, which rejects it and aborts the whole command.
pub fn preview_4k_encode_args_ffmpeg(video_encoder: &str) -> String {
    format!(
        "-hide_banner -loglevel error -fflags +discardcorrupt+genpts \
         -analyzeduration 600000 -probesize 1000000 \
         -skip_loop_filter:v all -skip_frame:v noref \
         -f mpegts -i pipe:0 \
         -map 0:v:0 -map 0:a:0 -map 0:d? -copy_unknown \
         -vf scale=1280:720:flags=fast_bilinear \
         -c:v {video_encoder} {tuning} -b:v 3000k -maxrate 4500k -bufsize 6000k \
         -g 60 -aspect 16:9 -c:a aac -b:a 192k -ar 48000 -ac 2 \
         -af aresample=async=1:min_hard_comp=0.100000:first_pts=0 -c:d copy \
         -f mpegts -flush_packets 1 -muxdelay 0 -muxpreload 0 pipe:1",
        tuning = video_encoder_tuning(video_encoder)
    )
}

pub fn preview_4k_extra_args_is_auto_generated(extra_args: Option<&str>) -> bool {
    let Some(args) = extra_args else { return true; };
    let args = args.trim();
    if args.is_empty() || args == DEFAULT_PREVIEW_4K_ENCODE_ARGS { return true; }
    KNOWN_PREVIEW_ENCODERS.iter().any(|enc| {
        args == preview_4k_encode_args_ffmpeg(enc)
            || KNOWN_HEVC_HW_DECODERS
                .iter()
                .any(|dec| args == preview_4k_encode_args_ffmpeg_hwdec(enc, dec))
            || legacy_preview_4k_encode_args_ffmpeg(enc)
                .iter()
                .any(|legacy| args == legacy.as_str())
    })
}

fn preview_video_encoder_from_auto_generated(extra_args: Option<&str>) -> Option<&'static str> {
    let args = extra_args?.trim();
    KNOWN_PREVIEW_ENCODERS
        .iter()
        .copied()
        .find(|enc| args == preview_encode_args_ffmpeg(enc))
}

/// エンコーダごとの最適化オプション。
///
/// ここで渡すものは [`preview_encode_args_ffmpeg`] が組み立てる本番の引数と
/// **完全に同じもの**を、`preview_setup` の選定時のテストエンコードでも使う。
/// `-c:v <名前>` だけ試して本番で別のオプションを足すと、そのオプションが
/// 効かないビルドだったときに「選定は通ったのに視聴開始で落ちる」ことになる。
///
/// 共通の狙いは**低遅延**。プレビューは「今映っているものを確認する」用途で、
/// 数秒の先読みバッファを積んで画質を稼ぐ意味がない。
///
/// - `libx264`: `veryfast` + `zerolatency` (Bフレームと先読みを止める)
/// - `h264_videotoolbox`: `-realtime 1` でリアルタイム優先、`-allow_sw 1` で
///   ハードウェアが埋まっているときにソフトウェアへ落として止まらないようにする
/// - `h264_qsv`: `-look_ahead 0` と `-async_depth 1`。先読みは遅延に直結する
/// - `h264_nvenc`: `-preset p4 -tune ll` (低遅延プリセット) + `-rc vbr`
/// - `h264_amf`: `-usage lowlatency -quality speed`
/// - `h264_vaapi`: `-vaapi_device` と `hwupload` フィルタの用意が要り、この
///   テンプレートの形では動かせない。オプションは足さず、選定時のテスト
///   エンコードで落ちて `libx264` にフォールバックさせる
pub fn video_encoder_tuning(video_encoder: &str) -> &'static str {
    match video_encoder {
        "libx264" => "-preset veryfast -tune zerolatency -profile:v high",
        "h264_videotoolbox" => "-realtime 1 -allow_sw 1 -profile:v high",
        "h264_qsv" => "-preset veryfast -look_ahead 0 -async_depth 1 -profile:v high",
        "h264_nvenc" => "-preset p4 -tune ll -rc vbr -profile:v high",
        "h264_amf" => "-usage lowlatency -quality speed -profile:v high",
        _ => "",
    }
}

/// この値は「利用者が自分で書いたもの」か、それとも「こちらが自動で入れた
/// ものか」。自動セットアップは後者しか上書きしない — 手で調整した引数を
/// 黙って踏み潰すのが一番やってはいけないことなので。
///
/// 自動生成とみなすのは、QSVEncC の初期シード、その前のレガシーテンプレート、
/// そして過去に自動セットアップが入れた ffmpeg テンプレート (どのエンコーダで
/// 生成されたものでも)。
pub fn preview_extra_args_is_auto_generated(extra_args: Option<&str>) -> bool {
    let Some(args) = extra_args else {
        // 未設定はそのまま入れてよい。
        return true;
    };
    let args = args.trim();
    if args.is_empty() {
        return true;
    }
    if args == DEFAULT_PREVIEW_ENCODE_ARGS || args == LEGACY_PREVIEW_ENCODE_ARGS {
        return true;
    }
    // 過去の自動生成 ffmpeg テンプレートかどうかは、エンコーダ名を差し替えた
    // 全候補と突き合わせて判定する。エンコーダが増えたらここに足すこと。
    KNOWN_PREVIEW_ENCODERS
        .iter()
        .any(|enc| args == preview_encode_args_ffmpeg(enc))
}

impl Database {
    fn row_to_encode_profile_record(row: &rusqlite::Row) -> rusqlite::Result<EncodeProfileRecord> {
        Ok(EncodeProfileRecord {
            id: row.get("id")?,
            name: row.get("name")?,
            purpose: row.get("purpose")?,
            codec: row.get("codec")?,
            container: row.get::<_, Option<String>>("container")?.unwrap_or_else(|| "mpegts".to_string()),
            target_bitrate: row.get("target_bitrate")?,
            extra_args: row.get("extra_args")?,
            is_enabled: row.get::<_, i64>("is_enabled")? != 0,
            created_at: row.get("created_at")?,
        })
    }

    /// All encode profiles, ordered by purpose then id.
    pub fn get_all_encode_profiles(&self) -> Result<Vec<EncodeProfileRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM encode_profiles ORDER BY purpose, id")?;
        let rows = stmt
            .query_map([], Self::row_to_encode_profile_record)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get a single encode profile by id.
    pub fn get_encode_profile(&self, id: i64) -> Result<Option<EncodeProfileRecord>> {
        let mut stmt = self.conn.prepare("SELECT * FROM encode_profiles WHERE id = ?1")?;
        Ok(stmt
            .query_row([id], Self::row_to_encode_profile_record)
            .optional()?)
    }

    /// First *enabled* profile for `purpose`, ordered by id ascending (so the
    /// seeded default profile wins unless an admin inserts one with a lower
    /// id — in practice the seed always has the lowest id for its purpose).
    ///
    /// Used by the HTTP preview streaming endpoint (STREAMING_DESIGN.md §6.3)
    /// to pick the encode profile for `?profile=preview`.
    pub fn get_encode_profile_by_purpose(&self, purpose: &str) -> Result<Option<EncodeProfileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM encode_profiles WHERE purpose = ?1 AND is_enabled = 1 ORDER BY id ASC LIMIT 1",
        )?;
        Ok(stmt
            .query_row(params![purpose], Self::row_to_encode_profile_record)
            .optional()?)
    }

    /// Insert a new encode profile. Returns the new row id.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_encode_profile(
        &self,
        name: &str,
        purpose: &str,
        codec: &str,
        container: &str,
        target_bitrate: Option<i64>,
        extra_args: Option<&str>,
        is_enabled: bool,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO encode_profiles (name, purpose, codec, container, target_bitrate, extra_args, is_enabled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                name,
                purpose,
                codec,
                container,
                target_bitrate,
                extra_args,
                is_enabled as i64
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Partially update an encode profile. Only `Some(..)` fields are
    /// changed; `target_bitrate`/`extra_args` use the `Option<Option<..>>`
    /// convention from `update_channel_full` (outer `None` = leave alone,
    /// `Some(None)` = clear to NULL, `Some(Some(x))` = set).
    #[allow(clippy::too_many_arguments)]
    pub fn update_encode_profile(
        &self,
        id: i64,
        name: Option<&str>,
        purpose: Option<&str>,
        codec: Option<&str>,
        container: Option<&str>,
        target_bitrate: Option<Option<i64>>,
        extra_args: Option<Option<&str>>,
        is_enabled: Option<bool>,
    ) -> Result<()> {
        let mut updates: Vec<&str> = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(v) = name {
            updates.push("name = ?");
            values.push(Box::new(v.to_string()));
        }
        if let Some(v) = purpose {
            updates.push("purpose = ?");
            values.push(Box::new(v.to_string()));
        }
        if let Some(v) = codec {
            updates.push("codec = ?");
            values.push(Box::new(v.to_string()));
        }
        if let Some(v) = container {
            updates.push("container = ?");
            values.push(Box::new(v.to_string()));
        }
        if let Some(v) = target_bitrate {
            updates.push("target_bitrate = ?");
            values.push(Box::new(v));
        }
        if let Some(v) = extra_args {
            updates.push("extra_args = ?");
            values.push(Box::new(v.map(|s| s.to_string())));
        }
        if let Some(v) = is_enabled {
            updates.push("is_enabled = ?");
            values.push(Box::new(v as i64));
        }

        if updates.is_empty() {
            return Ok(());
        }

        values.push(Box::new(id));
        let sql = format!("UPDATE encode_profiles SET {} WHERE id = ?", updates.join(", "));
        let params: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        self.conn.execute(&sql, params.as_slice())?;
        Ok(())
    }

    /// Delete an encode profile.
    pub fn delete_encode_profile(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM encode_profiles WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Seed the default `preview-h264` profile if no row with that name
    /// exists yet. Idempotent — safe to call on every startup.
    ///
    /// STREAMING_DESIGN.md §6.2/§12-2: preview is H.264 fixed (compatibility
    /// over quality — MSE/mpegts.js in mainstream browsers requires H.264 or
    /// HEVC-with-limited-support; H.264 is the safe default). `extra_args` is
    /// [`DEFAULT_PREVIEW_ENCODE_ARGS`], a direct QSVEncC template meant to run
    /// behind the seeded tsreadex preprocessor; sites without QSVEncC will
    /// need to edit this via the dashboard/API before `?profile=preview`
    /// produces playable output.
    pub(crate) fn seed_default_encode_profiles(&self) -> Result<()> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM encode_profiles WHERE name = ?1)",
            params!["preview-h264"],
            |row| row.get(0),
        )?;
        if !exists {
            self.insert_encode_profile(
                "preview-h264",
                "preview",
                "h264",
                "mpegts",
                Some(2_000_000),
                Some(DEFAULT_PREVIEW_ENCODE_ARGS),
                true,
            )?;
            log::info!("Seeded default encode profile 'preview-h264' (H.264, ~2Mbps, purpose=preview)");
        } else {
            // One-time carry-over for rows still holding the pre-two-stage
            // tsreplace-wrapped template verbatim: that shape is broken now
            // that `[preview] command_path` points at the encoder itself, so
            // swap in the direct-QSVEncC recommendation. Rows the admin has
            // edited (any other value) are never touched.
            let updated = self.conn.execute(
                "UPDATE encode_profiles SET extra_args = ?1
                 WHERE name = 'preview-h264' AND extra_args = ?2",
                params![DEFAULT_PREVIEW_ENCODE_ARGS, LEGACY_PREVIEW_ENCODE_ARGS],
            )?;
            if updated > 0 {
                log::info!(
                    "Migrated encode profile 'preview-h264' from the legacy tsreplace-wrapped \
                     template to the direct QSVEncC template"
                );
            }
        }

        // BS4K needs its own template (see `DEFAULT_PREVIEW_4K_ENCODE_ARGS`).
        // Seeded separately and by name, so an admin who deletes or edits it
        // keeps their version across restarts, exactly like the row above.
        let four_k_exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM encode_profiles WHERE name = ?1)",
            params!["preview-4k"],
            |row| row.get(0),
        )?;
        if !four_k_exists {
            let extra_args = preview_4k_encode_args_ffmpeg("libx264");
            self.insert_encode_profile(
                "preview-4k",
                "preview4k",
                "h264",
                "mpegts",
                Some(4_000_000),
                Some(&extra_args),
                true,
            )?;
            log::info!(
                "Seeded default encode profile 'preview-4k' (1080p downscale, no deinterlace, purpose=preview4k)"
            );
        } else {
            // Exact-match migration preserves every administrator-edited row.
            let preview_args: Option<String> = self
                .conn
                .query_row(
                    "SELECT extra_args FROM encode_profiles WHERE name = 'preview-h264' LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let encoder = preview_video_encoder_from_auto_generated(preview_args.as_deref())
                .unwrap_or("libx264");
            let replacement = preview_4k_encode_args_ffmpeg(encoder);
            // Any value this codebase generated itself is fair game to
            // replace: the rigaya seed that ffmpeg cannot parse at all, and
            // the first ffmpeg template that could not keep up with 2160p60.
            // An administrator's own edit matches neither and is left alone.
            let current: Option<String> = self
                .conn
                .query_row(
                    "SELECT extra_args FROM encode_profiles WHERE name = 'preview-4k' LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            if preview_4k_extra_args_is_auto_generated(current.as_deref())
                && current.as_deref() != Some(replacement.as_str())
            {
                self.conn.execute(
                    "UPDATE encode_profiles SET extra_args = ?1 WHERE name = 'preview-4k'",
                    params![replacement],
                )?;
                log::info!(
                    "Migrated encode profile 'preview-4k' to the current ffmpeg template"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::database::Database;

    #[test]
    fn seed_creates_default_preview_profile() {
        let db = Database::open_in_memory().unwrap();
        let profile = db
            .get_encode_profile_by_purpose("preview")
            .unwrap()
            .expect("seeded preview profile should exist");
        assert_eq!(profile.name, "preview-h264");
        assert_eq!(profile.codec, "h264");
        assert_eq!(profile.purpose, "preview");
        assert!(profile.is_enabled);
        // The recommendation is a direct QSVEncC invocation (two-stage design:
        // service selection lives in the tsreadex preprocessor arguments).
        assert_eq!(profile.extra_args.as_deref(), Some(super::DEFAULT_PREVIEW_ENCODE_ARGS));
        assert!(
            !profile.extra_args.unwrap().contains("--preserve-other-services"),
            "seed must not be tsreplace-shaped anymore"
        );
    }

    #[test]
    fn seed_migrates_untouched_legacy_template_but_not_edited_rows() {
        let db = Database::open_in_memory().unwrap();

        // Simulate a DB seeded by the pre-two-stage code: the row still holds
        // the old tsreplace-wrapped template verbatim.
        db.connection()
            .execute(
                "UPDATE encode_profiles SET extra_args = ?1 WHERE name = 'preview-h264'",
                rusqlite::params![super::LEGACY_PREVIEW_ENCODE_ARGS],
            )
            .unwrap();
        db.seed_default_encode_profiles().unwrap();
        let profile = db.get_encode_profile_by_purpose("preview").unwrap().unwrap();
        assert_eq!(profile.extra_args.as_deref(), Some(super::DEFAULT_PREVIEW_ENCODE_ARGS));

        // An admin-edited value must survive re-seeding untouched.
        db.update_encode_profile(profile.id, None, None, None, None, None, Some(Some("--custom")), None)
            .unwrap();
        db.seed_default_encode_profiles().unwrap();
        let profile = db.get_encode_profile_by_purpose("preview").unwrap().unwrap();
        assert_eq!(profile.extra_args.as_deref(), Some("--custom"));
    }

    #[test]
    fn seed_is_idempotent() {
        let db = Database::open_in_memory().unwrap();
        db.seed_default_encode_profiles().unwrap(); // called again on top of open()'s own seed
        let all = db.get_all_encode_profiles().unwrap();
        assert_eq!(all.iter().filter(|p| p.name == "preview-h264").count(), 1);
        assert_eq!(all.iter().filter(|p| p.name == "preview-4k").count(), 1);
    }

    #[test]
    fn four_k_ffmpeg_args_preserve_pipeline_without_deinterlacing() {
        let args = super::preview_4k_encode_args_ffmpeg("h264_qsv");
        assert!(args.contains("-f mpegts -i pipe:0"));
        assert!(args.contains("-map 0:d?"));
        assert!(args.contains("-c:d copy"));
        assert!(args.contains("-vf scale=1280:720"));
        assert!(args.contains("-c:v h264_qsv"));
        assert!(!args.contains("yadif"));
        assert!(!args.contains("--avhw"));
    }

    /// Software HEVC 2160p60 decode is the bottleneck, not the encoder, and
    /// no hardware decoder is available on the verification machine. The
    /// decode-side throttles are what get the preview to realtime, and both
    /// must be scoped to `:v` — an unscoped `-skip_frame` is handed to the
    /// AAC decoder too, which rejects it and aborts the whole command.
    #[test]
    fn four_k_args_throttle_the_decoder_and_scope_it_to_video() {
        let args = super::preview_4k_encode_args_ffmpeg("h264_qsv");
        assert!(args.contains("-skip_loop_filter:v all"), "{args}");
        assert!(args.contains("-skip_frame:v noref"), "{args}");
        assert!(!args.contains("-skip_frame noref"), "{args}");
        assert!(!args.contains("-skip_loop_filter all"), "{args}");
        // The throttles are decoder options: they only take effect before -i.
        let input_at = args.find("-i pipe:0").expect("input");
        assert!(args.find("-skip_frame:v").unwrap() < input_at, "{args}");
        assert!(args.find("-skip_loop_filter:v").unwrap() < input_at, "{args}");
    }

    /// Every earlier ffmpeg 4K template (0.27x, then 0.88x sustained) was
    /// generated by this codebase, so the seed must upgrade a row still
    /// holding one rather than treat it as an administrator's choice.
    #[test]
    fn seed_upgrades_every_superseded_ffmpeg_four_k_template() {
        for slow in super::legacy_preview_4k_encode_args_ffmpeg("libx264") {
            let db = Database::open_in_memory().unwrap();
            let profile = db.get_encode_profile_by_purpose("preview4k").unwrap().unwrap();
            db.update_encode_profile(profile.id, None, None, None, None, None,
                Some(Some(&slow)), None).unwrap();
            assert!(super::preview_4k_extra_args_is_auto_generated(Some(&slow)));
            db.seed_default_encode_profiles().unwrap();
            let migrated = db.get_encode_profile_by_purpose("preview4k").unwrap().unwrap();
            let args = migrated.extra_args.as_deref().unwrap();
            assert!(args.contains("-skip_frame:v noref"), "{args}");
            assert!(args.contains("scale=1280:720"), "{args}");
        }
    }

    #[test]
    fn seed_migrates_only_untouched_broken_four_k_template() {
        let db = Database::open_in_memory().unwrap();
        let ordinary = db.get_encode_profile_by_purpose("preview").unwrap().unwrap();
        let ordinary_args = super::preview_encode_args_ffmpeg("h264_qsv");
        db.update_encode_profile(ordinary.id, None, None, None, None, None,
            Some(Some(&ordinary_args)), None).unwrap();
        let profile = db.get_encode_profile_by_purpose("preview4k").unwrap().unwrap();
        db.update_encode_profile(profile.id, None, None, None, None, None,
            Some(Some(super::DEFAULT_PREVIEW_4K_ENCODE_ARGS)), None).unwrap();
        db.seed_default_encode_profiles().unwrap();
        let migrated = db.get_encode_profile_by_purpose("preview4k").unwrap().unwrap();
        assert_eq!(migrated.extra_args.as_deref(),
            Some(super::preview_4k_encode_args_ffmpeg("h264_qsv").as_str()));

        db.update_encode_profile(migrated.id, None, None, None, None, None,
            Some(Some("--avhw --administrator-customized")), None).unwrap();
        db.seed_default_encode_profiles().unwrap();
        let preserved = db.get_encode_profile_by_purpose("preview4k").unwrap().unwrap();
        assert_eq!(preserved.extra_args.as_deref(), Some("--avhw --administrator-customized"));
    }

    #[test]
    fn crud_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .insert_encode_profile("record-hevc", "record", "hevc", "mpegts", Some(8_000_000), Some("--foo"), true)
            .unwrap();

        let fetched = db.get_encode_profile(id).unwrap().expect("just inserted");
        assert_eq!(fetched.name, "record-hevc");
        assert_eq!(fetched.target_bitrate, Some(8_000_000));

        db.update_encode_profile(id, Some("record-hevc-v2"), None, None, None, Some(Some(10_000_000)), None, Some(false))
            .unwrap();
        let updated = db.get_encode_profile(id).unwrap().unwrap();
        assert_eq!(updated.name, "record-hevc-v2");
        assert_eq!(updated.target_bitrate, Some(10_000_000));
        assert!(!updated.is_enabled);

        db.delete_encode_profile(id).unwrap();
        assert!(db.get_encode_profile(id).unwrap().is_none());
    }

    #[test]
    fn get_by_purpose_ignores_disabled_rows() {
        let db = Database::open_in_memory().unwrap();
        // Disable the seeded default and insert a second, disabled-by-default
        // candidate — get_by_purpose must return None, not the disabled row.
        let seeded = db.get_encode_profile_by_purpose("preview").unwrap().unwrap();
        db.update_encode_profile(seeded.id, None, None, None, None, None, None, Some(false))
            .unwrap();
        assert!(db.get_encode_profile_by_purpose("preview").unwrap().is_none());
    }
}


//! セットアップ時のエンコーダ検出・動作確認と引数生成。
//!
//! 外部コマンドの検出結果は推測で採用しない。rigaya 系は `--check-hw`、
//! ffmpeg は実際のエンコードまで通った候補だけを採用する。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::database;

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const PIPELINE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RigayaKind {
    Qsv,
    Nvenc,
    Vce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsreplaceQuality {
    Compatibility,
    Compression,
}

impl RigayaKind {
    fn names(self) -> (&'static [&'static str], &'static str) {
        match self {
            Self::Qsv => (&["QSVEncC64.exe", "qsvencc"], "QSVEncC"),
            Self::Nvenc => (&["NVEncC64.exe", "nvencc"], "NVEncC"),
            Self::Vce => (&["VCEEncC64.exe", "vceencc"], "VCEEncC"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RigayaCandidate {
    pub path: PathBuf,
    pub kind: RigayaKind,
}

#[derive(Debug, Clone)]
pub struct FfmpegEncoderSpec {
    pub name: String,
    pub pre_input: Vec<String>,
    pub filter_suffix: String,
    pub tuning: &'static str,
}

#[derive(Debug, Clone)]
pub struct EncoderSelection {
    pub arguments: String,
    pub encoder: String,
    pub codec: String,
    pub reason: String,
    pub warnings: Vec<String>,
}

fn path_executable(path: &Path) -> bool {
    path.is_file()
}

fn absolute_path(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| path_executable(candidate))
        .map(absolute_path)
}

fn find_named_at_depth(root: &Path, names: &[&str], max_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| names.iter().any(|candidate| candidate.eq_ignore_ascii_case(name)))
            {
                found.push(path);
            } else if path.is_dir() && depth < max_depth {
                stack.push((path, depth + 1));
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

fn thirdparty_roots(install_dir: &Path, kind: RigayaKind) -> Vec<PathBuf> {
    let component = match kind {
        RigayaKind::Qsv => "QSVEncC",
        RigayaKind::Nvenc => "NVEncC",
        RigayaKind::Vce => "VCEEncC",
    };
    let mut roots = vec![install_dir.join("thirdparty").join(component)];
    if cfg!(target_os = "windows") {
        roots.push(PathBuf::from(format!(
            r"C:\DTV\KonomiTV\server\thirdparty\{component}"
        )));
    } else if cfg!(target_os = "linux") {
        roots.push(PathBuf::from(format!(
            "/opt/KonomiTV/server/thirdparty/{component}"
        )));
    }
    roots
}

pub fn detect_rigaya_encoders(install_dir: &Path) -> Vec<RigayaCandidate> {
    let mut result = Vec::new();
    for kind in [RigayaKind::Qsv, RigayaKind::Nvenc, RigayaKind::Vce] {
        let (names, _) = kind.names();
        for name in names {
            if let Some(path) = which(name) {
                result.push(RigayaCandidate {
                    path,
                    kind,
                });
            }
        }
        for root in thirdparty_roots(install_dir, kind) {
            for path in find_named_at_depth(&root, names, 2) {
                result.push(RigayaCandidate {
                    path: absolute_path(path),
                    kind,
                });
            }
        }
    }
    result.dedup_by(|a, b| a.path == b.path);
    result
}

fn configure_hidden_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{}秒でタイムアウトしました", timeout.as_secs()));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

pub fn check_rigaya_hw(candidate: &RigayaCandidate) -> Result<(), String> {
    let mut command = Command::new(&candidate.path);
    command.arg("--check-hw").stdout(Stdio::null()).stderr(Stdio::piped());
    configure_hidden_window(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("{} の起動に失敗: {error}", candidate.path.display()))?;
    let status = wait_with_timeout(&mut child, PROBE_TIMEOUT)?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("--check-hw が終了コード {:?} で失敗", status.code()))
    }
}

fn current_os_candidates() -> &'static [&'static str] {
    if cfg!(target_os = "windows") {
        &["hevc_qsv", "hevc_nvenc", "hevc_amf", "h264_qsv", "h264_nvenc", "h264_amf"]
    } else if cfg!(target_os = "macos") {
        &["hevc_videotoolbox", "h264_videotoolbox"]
    } else {
        &["hevc_qsv", "hevc_nvenc", "hevc_vaapi", "h264_qsv", "h264_nvenc", "h264_vaapi"]
    }
}

pub fn first_render_device() -> Option<PathBuf> {
    let dir = Path::new("/dev/dri");
    let mut devices = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect::<Vec<_>>();
    devices.sort();
    devices.into_iter().next()
}

pub fn ffmpeg_encoder_spec(name: &str) -> Option<FfmpegEncoderSpec> {
    let vaapi = name.ends_with("_vaapi");
    if vaapi && !cfg!(target_os = "linux") {
        return None;
    }
    let (pre_input, filter_suffix) = if vaapi {
        let device = first_render_device()?;
        (
            vec!["-vaapi_device".to_owned(), device.to_string_lossy().into_owned()],
            ",format=nv12,hwupload".to_owned(),
        )
    } else {
        (Vec::new(), String::new())
    };
    let tuning = database::video_encoder_tuning(name);
    if tuning.is_empty() && name != "h264_vaapi" && name != "hevc_vaapi" {
        return None;
    }
    Some(FfmpegEncoderSpec {
        name: name.to_owned(),
        pre_input,
        filter_suffix,
        tuning: if vaapi { "" } else { tuning },
    })
}

fn run_ffmpeg_probe(ffmpeg: &Path, args: &[String]) -> Result<ExitStatus, String> {
    let mut command = Command::new(ffmpeg);
    command.args(args).stdout(Stdio::null()).stderr(Stdio::null());
    configure_hidden_window(&mut command);
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    wait_with_timeout(&mut child, PROBE_TIMEOUT)
}

pub fn test_ffmpeg_encoder(ffmpeg: &Path, name: &str) -> Result<(), String> {
    let Some(spec) = ffmpeg_encoder_spec(name) else {
        return Err("このエンコーダの実行仕様を作れません".to_owned());
    };
    let mut args = vec!["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=d=0.2"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let insert_at = args.len() - 2;
    args.splice(insert_at..insert_at, spec.pre_input.clone());
    args.extend(["-vf".to_owned(), format!("format=yuv420p{}", spec.filter_suffix)]);
    args.extend(["-c:v".to_owned(), name.to_owned()]);
    if !spec.tuning.is_empty() {
        args.extend(spec.tuning.split_whitespace().map(str::to_owned));
    } else if name.starts_with("h264_") {
        args.extend(["-profile:v".to_owned(), "high".to_owned()]);
    }
    args.extend(["-f".to_owned(), "null".to_owned(), "-".to_owned()]);
    let status = run_ffmpeg_probe(ffmpeg, &args)?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("テストエンコードが終了コード {:?} で失敗", status.code()))
    }
}

pub fn listed_ffmpeg_candidates(encoders_output: &str) -> Vec<&'static str> {
    current_os_candidates()
        .iter()
        .copied()
        .filter(|name| encoders_output.lines().any(|line| line.split_whitespace().any(|token| token == *name)))
        .collect()
}

fn rigaya_arguments(kind: RigayaKind, path: &Path, codec: &str) -> String {
    let executable = path.to_string_lossy();
    let rate = if kind == RigayaKind::Nvenc {
        "--qvbr 23"
    } else {
        "--vbr 6000 --max-bitrate 12000"
    };
    format!(
        "-i - -o - --preserve-other-services -e {executable} -i - --input-format mpegts --tff --vpp-deinterlace normal -c {codec} {rate} --gop-len 90 --output-format mpegts -o -"
    )
}

fn ffmpeg_arguments(path: &Path, spec: &FfmpegEncoderSpec, codec: &str) -> String {
    let mut args = vec![
        "-i - -o - --preserve-other-services -e".to_owned(),
        path.to_string_lossy().into_owned(),
        "-y -hide_banner -loglevel error".to_owned(),
    ];
    args.extend(spec.pre_input.iter().cloned());
    args.extend([
        "-f mpegts -i - -copyts -start_at_zero -vf".to_owned(),
        format!("yadif=0:-1:1{}", spec.filter_suffix),
        format!("-an -c:v {codec}"),
    ]);
    if !spec.tuning.is_empty() {
        args.push(spec.tuning.to_owned());
    }
    if codec == "h264" && !spec.tuning.contains("-profile:v") {
        args.push("-profile:v high".to_owned());
    }
    args.push("-b:v 6000k -maxrate 12000k -bufsize 12000k -g 90 -f mpegts -".to_owned());
    args.join(" ")
}

pub fn tsreplace_arguments(kind: RigayaKind, encoder_path: &Path, codec: &str) -> Option<String> {
    if encoder_path.to_string_lossy().chars().any(char::is_whitespace) {
        return None;
    }
    Some(rigaya_arguments(kind, encoder_path, codec))
}

pub fn tsreplace_ffmpeg_arguments(ffmpeg_path: &Path, encoder: &str, codec: &str) -> Option<String> {
    if ffmpeg_path.to_string_lossy().chars().any(char::is_whitespace) {
        return None;
    }
    let spec = ffmpeg_encoder_spec(encoder)?;
    Some(ffmpeg_arguments(ffmpeg_path, &spec, codec))
}

fn normalized_arguments(arguments: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut skip = false;
    for token in arguments.split_whitespace() {
        if skip {
            skip = false;
            continue;
        }
        if token == "-e" {
            result.push(token.to_owned());
            skip = true;
        } else {
            result.push(token.to_owned());
        }
    }
    result
}

pub fn tsreplace_arguments_is_auto_generated(arguments: Option<&str>) -> bool {
    let Some(arguments) = arguments else { return true };
    let normalized = normalized_arguments(arguments.trim());
    if normalized.is_empty() {
        return true;
    }
    let defaults = normalized_arguments(database::default_tsreplace_arguments());
    if normalized == defaults {
        return true;
    }
    let placeholder = Path::new("/recisdb-auto/encoder");
    for kind in [RigayaKind::Qsv, RigayaKind::Nvenc, RigayaKind::Vce] {
        for codec in ["h264", "hevc"] {
            if tsreplace_arguments(kind, placeholder, codec)
                .is_some_and(|value| normalized == normalized_arguments(&value))
            {
                return true;
            }
        }
    }
    for encoder in [
        "libx264", "h264_qsv", "h264_nvenc", "h264_amf", "h264_vaapi", "h264_videotoolbox",
        "hevc_qsv", "hevc_nvenc", "hevc_vaapi", "hevc_videotoolbox",
    ] {
        for codec in ["h264", "hevc"] {
            let spec = FfmpegEncoderSpec {
                name: encoder.to_owned(),
                pre_input: Vec::new(),
                filter_suffix: String::new(),
                tuning: database::video_encoder_tuning(encoder),
            };
            let value = ffmpeg_arguments(placeholder, &spec, codec);
            if normalized == normalized_arguments(&value) {
                return true;
            }
        }
    }
    false
}

fn replace_io_tokens(arguments: &str, input: &Path, output: &Path) -> Option<Vec<String>> {
    let mut tokens = arguments.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
    let input_index = tokens.windows(2).position(|pair| pair == ["-i", "-"])?;
    let output_index = tokens.windows(2).position(|pair| pair == ["-o", "-"])?;
    tokens[input_index + 1] = input.to_string_lossy().into_owned();
    tokens[output_index + 1] = output.to_string_lossy().into_owned();
    Some(tokens)
}

fn temp_work_dir() -> PathBuf {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("recisdb-encoder-probe-{}-{suffix}", std::process::id()))
}

fn make_sample_ts(ffmpeg: &Path, dir: &Path) -> Result<PathBuf, String> {
    let sample = dir.join("sample.ts");
    let args = [
        "-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i",
        "testsrc2=size=1440x1080:rate=30:d=2", "-f", "lavfi", "-i", "sine=frequency=1000:duration=2",
        "-vf", "tinterlace=interleave_top", "-c:v", "mpeg2video", "-flags", "+ildct+ilme", "-top", "1",
        "-c:a", "aac", "-shortest", "-f", "mpegts",
    ];
    let mut command = Command::new(ffmpeg);
    command.args(args).arg(&sample).stdout(Stdio::null()).stderr(Stdio::null());
    configure_hidden_window(&mut command);
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let status = wait_with_timeout(&mut child, PIPELINE_TIMEOUT)?;
    if status.success() && sample.metadata().is_ok_and(|meta| meta.len() >= 188) {
        Ok(sample)
    } else {
        Err("試験用TSの生成に失敗しました".to_owned())
    }
}

pub fn test_tsreplace_pipeline(tsreplace: &Path, arguments: &str, ffmpeg: &Path) -> Result<(), String> {
    let dir = temp_work_dir();
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let result = (|| {
        let sample = make_sample_ts(ffmpeg, &dir)?;
        let output = dir.join("encoded.ts");
        let args = replace_io_tokens(arguments, &sample, &output)
            .ok_or_else(|| "tsreplace引数の入出力指定を解釈できません".to_owned())?;
        let mut command = Command::new(tsreplace);
        command.args(args).stdout(Stdio::null()).stderr(Stdio::null());
        configure_hidden_window(&mut command);
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        let status = wait_with_timeout(&mut child, PIPELINE_TIMEOUT)?;
        if status.success() && output.metadata().is_ok_and(|meta| meta.len() >= 188) {
            Ok(())
        } else {
            Err(format!("tsreplace試験が終了コード {:?} で失敗", status.code()))
        }
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

pub fn select_tsreplace_arguments(
    install_dir: &Path,
    tsreplace_path: &Path,
    quality: TsreplaceQuality,
) -> Result<EncoderSelection, String> {
    let mut warnings = Vec::new();
    let mut failures = Vec::new();
    let codecs = match quality {
        TsreplaceQuality::Compatibility => vec!["h264"],
        TsreplaceQuality::Compression => vec!["hevc", "h264"],
    };
    let ffmpeg = match crate::preview_setup::resolve_ffmpeg(install_dir) {
        Ok((path, _, listing)) => Some((absolute_path(path), listing)),
        Err(error) => {
            warnings.push(format!("ffmpegの検出・取得に失敗しました。単体試験を省略します: {error}"));
            None
        }
    };
    for codec in &codecs {
        for candidate in detect_rigaya_encoders(install_dir) {
            if candidate.path.to_string_lossy().chars().any(char::is_whitespace) {
                failures.push(format!("{}: パスに空白があるため除外", candidate.path.display()));
                continue;
            }
            if let Err(error) = check_rigaya_hw(&candidate) {
                failures.push(format!("{}: {error}", candidate.path.display()));
                continue;
            }
            let Some(arguments) = tsreplace_arguments(candidate.kind, &candidate.path, codec) else {
                failures.push(format!("{}: 引数生成に失敗", candidate.path.display()));
                continue;
            };
            if let Some((ffmpeg_path, _)) = &ffmpeg {
                if let Err(error) = test_tsreplace_pipeline(&candidate.path, &arguments, ffmpeg_path) {
                    failures.push(format!("{}: パイプライン試験失敗: {error}", candidate.path.display()));
                    continue;
                }
            } else {
                warnings.push(format!("{} は --check-hw のみで採用しました。試験用TSを作るffmpegがありません", candidate.path.display()));
            }
            return Ok(EncoderSelection {
                arguments,
                encoder: candidate.path.display().to_string(),
                codec: (*codec).to_owned(),
                reason: format!("{} の --check-hw 成功", candidate.path.display()),
                warnings: failures,
            });
        }
    }

    let Some((ffmpeg, listing)) = ffmpeg else {
        warnings.extend(failures);
        return Err(if warnings.is_empty() {
            "利用可能なrigaya/ffmpegエンコーダがありません".to_owned()
        } else {
            format!("利用可能なエンコーダがありません: {}", warnings.join("; "))
        });
    };
    for codec in &codecs {
        for encoder in current_os_candidates()
            .iter()
            .copied()
            .filter(|name| name.starts_with(if *codec == "hevc" { "hevc_" } else { "h264_" }))
            .filter(|name| listing.contains(*name))
        {
            if let Err(error) = test_ffmpeg_encoder(&ffmpeg, encoder) {
                failures.push(format!("ffmpeg {encoder}: {error}"));
                continue;
            }
            let Some(arguments) = tsreplace_ffmpeg_arguments(&ffmpeg, encoder, codec) else {
                failures.push(format!("ffmpeg {encoder}: 実行ファイルパスまたはspecを作れない"));
                continue;
            };
            if let Err(error) = test_tsreplace_pipeline(&tsreplace_path, &arguments, &ffmpeg) {
                failures.push(format!("ffmpeg {encoder}: パイプライン試験失敗: {error}"));
                continue;
            }
            return Ok(EncoderSelection {
                arguments,
                encoder: ffmpeg.display().to_string(),
                codec: (*codec).to_owned(),
                reason: format!("ffmpeg {encoder} の単体・パイプライン試験成功"),
                warnings: failures,
            });
        }
    }
    if codecs.iter().any(|codec| *codec == "h264") {
        if test_ffmpeg_encoder(&ffmpeg, "libx264").is_ok() {
            if let Some(arguments) = tsreplace_ffmpeg_arguments(&ffmpeg, "libx264", "h264") {
                if test_tsreplace_pipeline(tsreplace_path, &arguments, &ffmpeg).is_ok() {
                    return Ok(EncoderSelection {
                        arguments,
                        encoder: ffmpeg.display().to_string(),
                        codec: "h264".to_owned(),
                        reason: "ffmpeg libx264 の単体・パイプライン試験成功".to_owned(),
                        warnings: failures,
                    });
                }
            }
        }
    }
    warnings.extend(failures);
    Err(if warnings.is_empty() {
        "利用可能なrigaya/ffmpegエンコーダがありません".to_owned()
    } else {
        format!("利用可能なエンコーダがありません: {}", warnings.join("; "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_arguments_cover_rigaya_variants() {
        let path = Path::new("/opt/QSVEncC");
        assert!(tsreplace_arguments(RigayaKind::Qsv, path, "h264").is_some_and(|v| v.contains("--vbr 6000")));
        assert!(tsreplace_arguments(RigayaKind::Nvenc, path, "hevc").is_some_and(|v| v.contains("--qvbr 23")));
        assert!(tsreplace_arguments(RigayaKind::Vce, path, "h264").is_some_and(|v| v.contains("-c h264")));
    }

    #[test]
    fn paths_with_whitespace_are_excluded() {
        assert!(tsreplace_arguments(RigayaKind::Qsv, Path::new("/tmp/has space/QSVEncC"), "h264").is_none());
    }

    #[test]
    fn default_and_generated_arguments_are_auto_but_manual_is_not() {
        assert!(tsreplace_arguments_is_auto_generated(Some(database::default_tsreplace_arguments())));
        let generated = tsreplace_arguments(RigayaKind::Qsv, Path::new("/opt/QSVEncC"), "h264").expect("generated");
        assert!(tsreplace_arguments_is_auto_generated(Some(&generated)));
        assert!(!tsreplace_arguments_is_auto_generated(Some("-i - -o - --my-custom-setting")));
    }

    #[cfg(unix)]
    #[test]
    fn check_hw_accepts_zero_and_rejects_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("recisdb-probe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let ok = dir.join("ok");
        let ng = dir.join("ng");
        std::fs::write(&ok, "#!/bin/sh\nexit 0\n").expect("ok");
        std::fs::write(&ng, "#!/bin/sh\nexit 1\n").expect("ng");
        for path in [&ok, &ng] {
            let mut mode = std::fs::metadata(path).expect("metadata").permissions();
            mode.set_mode(0o755);
            std::fs::set_permissions(path, mode).expect("chmod");
        }
        assert!(check_rigaya_hw(&RigayaCandidate { path: ok.clone(), kind: RigayaKind::Qsv }).is_ok());
        assert!(check_rigaya_hw(&RigayaCandidate { path: ng.clone(), kind: RigayaKind::Qsv }).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}

//! recisdb-proxy かんたんセットアップ (GUIウィザード)
//!
//! プログラムを触ったことがない人でも、画面の指示に従って「次へ」を押すだけで
//! recisdb-proxy を使い始められることを目標にしたセットアップウィザード。
//! コマンドライン入力は一切不要。実際のロジック(チューナー検出・設定ファイル
//! 生成・DB登録)は `recisdb_proxy::setup_helpers` に切り出されている。
//!
//! 起動直後にセットアップの種類 ([`SetupMode`]) を選ぶ。用途が違う3つの作業を
//! 1本のウィザードに押し込むと、DLLを更新したいだけの人にもチューナー検出や
//! サービス登録の画面を通らせることになるため、入口で分岐させている。
//!
//! - [`SetupMode::FullAuto`]    … 本体インストール(全自動)。ドライバ導入まで自動
//! - [`SetupMode::Manual`]      … 本体インストール(ドライバ導入をスキップ、手動設定)
//! - [`SetupMode::DllOnly`]     … クライアントDLLの差し替えのみ
// リリースビルドでは黒いコンソール窓を出さない(デバッグ時は println! を見たいので
// デバッグビルドではコンソールを残す)。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui;
use recisdb_proxy::config_file::{self, TomlValue};
use recisdb_proxy::database::Database;
use recisdb_proxy::database::StreamFormat;
use recisdb_proxy::encoder_probe::{self, TsreplaceQuality};
use recisdb_proxy::fourk_setup::{self, AcasSelection};
use recisdb_proxy::px4_installer;
use recisdb_proxy::setup_helpers::{
    self, bulk_update_bondriver_dlls, generate_config, register_manual_tuner,
    register_tuners_to_db, DetectedTuner,
};

/// px4_drv 自動インストールのバックグラウンドスレッドから届く通知。
enum InstallEvent {
    Progress(String),
    Done(Result<Vec<String>, String>),
}

enum SetupEvent {
    Progress(String),
    SmartCardReaders(Vec<String>),
    Done(Result<SetupResult, String>),
}

struct SetupResult {
    log_lines: Vec<String>,
    service_registered: bool,
}

struct SetupJob {
    install_dir: PathBuf,
    source_dir: Option<PathBuf>,
    config_path: PathBuf,
    db_path: PathBuf,
    setup_config: setup_helpers::SetupConfig,
    overwrite_config: bool,
    recreate_db: bool,
    detected: Vec<DetectedTuner>,
    selected: Vec<bool>,
    manual_entries: Vec<ManualEntry>,
    setup_preview: bool,
    setup_tsreplace: bool,
    tsreplace_quality: TsreplaceQuality,
    setup_4k: bool,
    fourk_paths: Vec<PathBuf>,
    fourk_acas: AcasSelection,
    listen_addr: String,
    web_listen_addr: String,
    lan_access: bool,
    firewall_allow: bool,
    register_service: bool,
    service_name: String,
    service_user_scope: bool,
}

const WINDOW_TITLE: &str = "recisdb-proxy かんたんセットアップ";

fn main() -> eframe::Result {
    env_logger::init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // 既定サイズを大きめに取る。文字を大きくした分、以前の 620x480 では
            // 確認画面・完了画面がすぐスクロール必須になり操作しづらかった。
            .with_inner_size([980.0, 800.0])
            .with_min_inner_size([760.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(|cc| {
            let fonts = install_fonts(&cc.egui_ctx);
            apply_theme(&cc.egui_ctx, &fonts);
            Ok(Box::new(SetupApp::new()))
        }),
    )
}

// =============================================================================
// 外観 (フォント・配色・余白)
// =============================================================================

/// 画面配色。初めて使う人・年配の利用者が読みやすいことを最優先に、
/// 明るい背景 + 十分なコントラスト(本文は背景に対して 12:1 以上)で組む。
/// ダークテーマは用意しない(セットアップは一度きりの作業で、明るい画面の方が
/// 文字が読みやすいため)。
mod palette {
    use eframe::egui::Color32;

    /// ウィンドウ全体の背景 (ごく淡い青みのグレー)
    pub const BG: Color32 = Color32::from_rgb(0xF4, 0xF7, 0xFB);
    /// カード(情報のかたまり)の背景
    pub const CARD: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
    /// 入力欄の背景
    pub const FIELD: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
    /// 補助的な帯・見出し背景
    pub const FAINT: Color32 = Color32::from_rgb(0xEA, 0xF0, 0xF8);
    /// 本文の文字色
    pub const TEXT: Color32 = Color32::from_rgb(0x17, 0x1F, 0x2A);
    /// 補足説明の文字色 (背景に対して 4.5:1 以上を確保する)
    pub const MUTED: Color32 = Color32::from_rgb(0x4B, 0x58, 0x67);
    /// 主要操作の色
    pub const ACCENT: Color32 = Color32::from_rgb(0x0B, 0x5C, 0xAB);
    pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x0A, 0x4E, 0x93);
    pub const ACCENT_ACTIVE: Color32 = Color32::from_rgb(0x08, 0x41, 0x7A);
    /// 通常ボタンの面
    pub const BUTTON: Color32 = Color32::from_rgb(0xE7, 0xEE, 0xF7);
    pub const BUTTON_HOVER: Color32 = Color32::from_rgb(0xD8, 0xE4, 0xF3);
    pub const BUTTON_ACTIVE: Color32 = Color32::from_rgb(0xC7, 0xD8, 0xEE);
    /// 枠線
    pub const BORDER: Color32 = Color32::from_rgb(0xC9, 0xD4, 0xE1);
    /// 状態色
    pub const DANGER: Color32 = Color32::from_rgb(0xB3, 0x21, 0x1C);
    pub const WARN: Color32 = Color32::from_rgb(0x8A, 0x54, 0x00);
    pub const OK: Color32 = Color32::from_rgb(0x1B, 0x6B, 0x3A);
}

/// 読み込めた日本語フォントの状況。太字フォントを別ファミリとして登録できた
/// 場合のみ、見出しに太字を使う。
struct FontSetup {
    /// 太字ファミリ ("jp_bold") を登録できたか
    has_bold: bool,
}

/// 見出しに使うフォントファミリ名。
const BOLD_FAMILY: &str = "jp_bold";

/// 日本語が文字化けせず、かつ読み間違えにくいフォントを設定する。
///
/// 候補は **ユニバーサルデザインフォントを最優先** に並べる (BIZ UDゴシック /
/// UDデジタル教科書体 / Noto Sans CJK)。UDフォントは濁点・半濁点や
/// 「ソ/ン」「シ/ツ」の判別がしやすく、初めて設定する人が値を読み違えにくい。
/// 見つからない場合は従来どおりOS標準の和文フォントへ落とす(いずれも無い
/// 場合はegui標準フォントのままとなり和文は豆腐になるが、起動不能よりはまし)。
fn install_fonts(ctx: &egui::Context) -> FontSetup {
    let mut fonts = egui::FontDefinitions::default();

    // (通常フォント, 対になる太字フォント) の候補。上から順に試す。
    let candidates: &[(&str, &str)] = if cfg!(windows) {
        &[
            // BIZ UDゴシック (Windows 10 1809 以降に標準搭載)
            (
                "C:\\Windows\\Fonts\\BIZ-UDGothicR.ttc",
                "C:\\Windows\\Fonts\\BIZ-UDGothicB.ttc",
            ),
            // UDデジタル教科書体 (同上)
            (
                "C:\\Windows\\Fonts\\UDDigiKyokashoN-R.ttc",
                "C:\\Windows\\Fonts\\UDDigiKyokashoN-B.ttc",
            ),
            // 以降はUDではないが可読性の高い順
            (
                "C:\\Windows\\Fonts\\YuGothM.ttc",
                "C:\\Windows\\Fonts\\YuGothB.ttc",
            ),
            (
                "C:\\Windows\\Fonts\\meiryo.ttc",
                "C:\\Windows\\Fonts\\meiryob.ttc",
            ),
            (
                "C:\\Windows\\Fonts\\msgothic.ttc",
                "C:\\Windows\\Fonts\\msgothic.ttc",
            ),
        ]
    } else if cfg!(target_os = "macos") {
        &[
            (
                "/System/Library/Fonts/ヒラギノ角ゴシック W4.ttc",
                "/System/Library/Fonts/ヒラギノ角ゴシック W7.ttc",
            ),
            (
                "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
                "/System/Library/Fonts/ヒラギノ角ゴシック W6.ttc",
            ),
        ]
    } else {
        &[
            (
                "/usr/share/fonts/truetype/BIZUDGothic/BIZUDGothic-Regular.ttf",
                "/usr/share/fonts/truetype/BIZUDGothic/BIZUDGothic-Bold.ttf",
            ),
            (
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc",
            ),
            (
                "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/truetype/noto/NotoSansCJK-Bold.ttc",
            ),
        ]
    };

    for (regular, bold) in candidates {
        let Ok(bytes) = std::fs::read(regular) else {
            continue;
        };
        fonts
            .font_data
            .insert("jp".to_owned(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "jp".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("jp".to_owned());

        // 太字は取れなくても致命的ではないので、失敗しても通常フォントで続行する。
        let has_bold = match std::fs::read(bold) {
            Ok(bold_bytes) if bold != regular => {
                fonts.font_data.insert(
                    "jp_bold".to_owned(),
                    egui::FontData::from_owned(bold_bytes).into(),
                );
                fonts.families.insert(
                    egui::FontFamily::Name(BOLD_FAMILY.into()),
                    vec!["jp_bold".to_owned(), "jp".to_owned()],
                );
                true
            }
            _ => false,
        };

        ctx.set_fonts(fonts);
        return FontSetup { has_bold };
    }

    FontSetup { has_bold: false }
}

/// 見出し用フォントファミリ。太字を読み込めていればそれを使う。
fn heading_family(fonts: &FontSetup) -> egui::FontFamily {
    if fonts.has_bold {
        egui::FontFamily::Name(BOLD_FAMILY.into())
    } else {
        egui::FontFamily::Proportional
    }
}

/// 文字サイズ・余白・配色をまとめて適用する。
///
/// 文字サイズは egui 既定 (本文 12.5pt 相当) では小さすぎるため、本文 17px /
/// 見出し 27px まで引き上げる。あわせてボタンの最小高さを 44px 確保し、
/// マウス操作に不慣れでも押しやすくする。
fn apply_theme(ctx: &egui::Context, fonts: &FontSetup) {
    use egui::{FontFamily, FontId, Style, TextStyle};

    // OS側がダークテーマでも、このウィザードは常に明るい配色で表示する
    // (下で組み立てる配色はライト前提。混ざると文字が読めなくなる)。
    ctx.set_theme(egui::ThemePreference::Light);

    let mut style = Style::default();

    style.text_styles = [
        (TextStyle::Heading, FontId::new(27.0, heading_family(fonts))),
        (TextStyle::Body, FontId::new(17.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(17.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(14.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(15.0, FontFamily::Monospace),
        ),
    ]
    .into();

    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(18.0, 10.0);
    style.spacing.interact_size = egui::vec2(48.0, 34.0);
    style.spacing.indent = 24.0;
    style.spacing.icon_width = 22.0;
    style.spacing.icon_width_inner = 12.0;
    style.spacing.scroll.bar_width = 12.0;
    style.spacing.text_edit_width = 360.0;

    let mut v = egui::Visuals::light();
    v.panel_fill = palette::BG;
    v.window_fill = palette::CARD;
    v.extreme_bg_color = palette::FIELD;
    v.faint_bg_color = palette::FAINT;
    v.code_bg_color = palette::FAINT;
    v.override_text_color = Some(palette::TEXT);
    v.weak_text_color = Some(palette::MUTED);
    v.hyperlink_color = palette::ACCENT;
    v.error_fg_color = palette::DANGER;
    v.warn_fg_color = palette::WARN;
    v.window_stroke = egui::Stroke::new(1.0, palette::BORDER);
    v.selection.bg_fill = palette::ACCENT.gamma_multiply(0.25);
    v.selection.stroke = egui::Stroke::new(1.0, palette::TEXT);

    let radius = egui::CornerRadius::same(8);
    v.widgets.noninteractive.bg_fill = palette::CARD;
    v.widgets.noninteractive.weak_bg_fill = palette::CARD;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, palette::BORDER);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, palette::TEXT);
    v.widgets.noninteractive.corner_radius = radius;

    v.widgets.inactive.bg_fill = palette::BUTTON;
    v.widgets.inactive.weak_bg_fill = palette::BUTTON;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, palette::BORDER);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, palette::TEXT);
    v.widgets.inactive.corner_radius = radius;

    v.widgets.hovered.bg_fill = palette::BUTTON_HOVER;
    v.widgets.hovered.weak_bg_fill = palette::BUTTON_HOVER;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(2.0, palette::ACCENT);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.5, palette::TEXT);
    v.widgets.hovered.corner_radius = radius;

    v.widgets.active.bg_fill = palette::BUTTON_ACTIVE;
    v.widgets.active.weak_bg_fill = palette::BUTTON_ACTIVE;
    v.widgets.active.bg_stroke = egui::Stroke::new(2.0, palette::ACCENT_ACTIVE);
    v.widgets.active.fg_stroke = egui::Stroke::new(2.0, palette::TEXT);
    v.widgets.active.corner_radius = radius;

    v.widgets.open.bg_fill = palette::FAINT;
    v.widgets.open.weak_bg_fill = palette::FAINT;
    v.widgets.open.bg_stroke = egui::Stroke::new(1.0, palette::BORDER);
    v.widgets.open.fg_stroke = egui::Stroke::new(1.0, palette::TEXT);
    v.widgets.open.corner_radius = radius;

    style.visuals = v;
    // ライト/ダーク両方に同じスタイルを入れておく (テーマ設定に関わらず
    // 同じ見た目になるようにする)。
    ctx.all_styles_mut(|s| *s = style.clone());
}

/// 情報のかたまりを白いカードにまとめる。項目の境目が分かりやすくなる。
fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(palette::CARD)
        .stroke(egui::Stroke::new(1.0, palette::BORDER))
        .corner_radius(10)
        .inner_margin(16)
        .show(ui, add)
        .inner
}

/// ページ見出し。「今どの段階なのか」を必ず添えて迷子を防ぐ。
fn page_title(ui: &mut egui::Ui, step_label: &str, title: &str) {
    if !step_label.is_empty() {
        ui.label(
            egui::RichText::new(step_label)
                .size(15.0)
                .color(palette::ACCENT),
        );
    }
    ui.heading(title);
    ui.add_space(4.0);
}

/// 主要操作(次へ・実行)のボタン。色と大きさで一目でそれと分かるようにする。
fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            egui::RichText::new(text)
                .size(18.0)
                .color(egui::Color32::WHITE),
        )
        .fill(palette::ACCENT)
        .corner_radius(8)
        .min_size(egui::vec2(200.0, 46.0)),
    )
}

/// 副次操作(戻る・終了など)のボタン。
fn secondary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(17.0))
            .corner_radius(8)
            .min_size(egui::vec2(150.0, 46.0)),
    )
}

/// 補足説明。本文より一段弱い色で、読み飛ばしても支障ない情報だと示す。
fn hint(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(15.0).color(palette::MUTED));
}

/// エラー表示。赤の帯で囲み、見落とされないようにする。
fn error_box(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(0xFD, 0xF2, 0xF1))
        .stroke(egui::Stroke::new(1.0, palette::DANGER))
        .corner_radius(8)
        .inner_margin(12)
        .show(ui, |ui| {
            ui.colored_label(palette::DANGER, text);
        });
}

// =============================================================================
// 画面遷移
// =============================================================================

/// セットアップの種類。起動直後に選ぶ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupMode {
    /// 本体インストール(全自動)。チューナー検出・ドライバ導入・プレビュー準備・
    /// サービス登録まで既定で全部行う。
    FullAuto,
    /// 本体インストール(ドライバ導入をスキップして手動設定)。
    /// 既にドライバを入れてある環境や、自動導入を避けたい環境向け。
    Manual,
    /// クライアントDLL (BonDriver_NetworkProxy*.dll) の差し替えのみ。
    /// 本体・設定・DBには一切触れない。
    DllOnly,
}

impl SetupMode {
    fn title(self) -> &'static str {
        match self {
            Self::FullAuto => "本体をインストール (全自動)",
            Self::Manual => "本体をインストール (ドライバ導入をスキップ)",
            Self::DllOnly => "クライアントDLLを差し替える",
        }
    }

    /// この画面が何ステップ目かの表示 (`step_of` は1始まり)。
    fn step_label(self, step_of: usize) -> String {
        let total = match self {
            Self::DllOnly => 1,
            _ => 4,
        };
        format!("{} ─ ステップ {step_of} / {total}", self.title())
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// セットアップの種類を選ぶ入口
    ModeSelect,
    Location,
    Detecting,
    SelectTuners,
    Options,
    Confirm,
    Done,
    /// DLL差し替え専用画面 ([`SetupMode::DllOnly`])
    DllOnly,
}

/// 手動追加されたチューナー1件分の入力欄
struct ManualEntryForm {
    path: String,
    group: String,
    max_instances: String,
}

impl Default for ManualEntryForm {
    fn default() -> Self {
        Self {
            path: String::new(),
            group: String::new(),
            max_instances: "1".to_string(),
        }
    }
}

#[derive(Clone)]
struct ManualEntry {
    path: String,
    group: String,
    max_instances: i32,
}

/// インストール先フォルダの既定値。
fn default_install_location() -> String {
    if cfg!(windows) {
        r"C:\DTV\recisdb-proxy-rs".to_string()
    } else {
        ".".to_string()
    }
}

fn default_node_display_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_default()
}

struct SetupApp {
    step: Step,
    /// 入口で選んだセットアップの種類
    mode: SetupMode,

    // ステップ1: 基本設定 (ほぼ既定値のまま「次へ」を押すだけで進める)
    listen_addr: String,
    web_listen_addr: String,
    /// Web/APIとチューナー接続をLANへ公開するか。
    lan_access: bool,
    /// recisdb-proxy 本体・設定ファイル・データベースを配置するフォルダ。
    /// 設定ファイル/DBのパスはここから常に導出する ([`SetupApp::config_file_path`] /
    /// [`SetupApp::db_file_path`])。
    install_location: String,

    /// 既存クライアントDLL (`BonDriver_NetworkProxy` 接頭辞) を一括更新する
    /// 対象フォルダ (インストール先とは別に指定できる、省略可)。
    /// 空のままなら完了画面での一括更新プロンプトを出さない。
    bulk_update_dir: String,
    /// 差し替え元にするDLLのパス。空なら [`SetupApp::resolve_source_dll`] が
    /// インストール先のクライアント配布フォルダ → このツールの隣、の順に探す。
    /// [`SetupMode::DllOnly`] では画面から明示指定できる。
    dll_source_path: String,

    // ステップ2: チューナー検出
    detect_rx: Option<mpsc::Receiver<Vec<DetectedTuner>>>,
    detected: Vec<DetectedTuner>,
    selected: Vec<bool>,
    manual_form: ManualEntryForm,
    manual_entries: Vec<ManualEntry>,

    // px4_drv 自動インストール (対応機種がドライバ未インストールで検出された場合)
    installing_index: Option<usize>,
    install_rx: Option<mpsc::Receiver<InstallEvent>>,
    install_log: Vec<String>,
    install_error: Option<String>,
    /// 自動インストールに失敗したチューナーの添字。全自動モードで次の対象を
    /// 選ぶときに、失敗したものを何度も再試行して止まらないようにする。
    driver_install_failed: Vec<usize>,

    // 上書き確認
    overwrite_config: bool,
    recreate_db: bool,

    // OSサービス登録 (service/mod.rs)。全自動モードでは既定でON: 常時稼働
    // させるのが想定利用形態のため。
    register_service: bool,
    /// ブラウザプレビューを使えるようにする (エンコーダと前段処理を自動で用意)。
    setup_preview: bool,
    /// TVTest向けtsreplaceを検出・準備する。
    setup_tsreplace: bool,
    tsreplace_quality: TsreplaceQuality,
    /// Windows BS4Kラッパーを構成する。
    setup_4k: bool,
    /// BS4Kラッパー化する基底BonDriverの候補と選択状態。
    fourk_paths: Vec<String>,
    fourk_selected: Vec<bool>,
    fourk_acas: AcasSelection,
    fourk_reader_candidates: Vec<String>,
    fourk_probe_rx: Option<mpsc::Receiver<Result<Vec<String>, String>>>,
    /// Windowsでプログラム単位の受信許可ルールを追加する。
    firewall_allow: bool,
    /// Mirakurun互換APIを有効にする。
    mirakurun_enabled: bool,
    /// Mirakurunの地元都道府県。空文字は未指定。
    home_region: String,
    /// ノード間通信で表示する名前。
    node_display_name: String,
    service_name: String,
    /// サービスとしての登録に成功したか。完了画面での「起動する」ボタンを
    /// 「ダッシュボードを開く」に切り替えるのに使う (サービスが既に
    /// listen しているので二重起動するとポートが衝突する)。
    service_registered: bool,
    /// ユーザー単位で登録する (systemd --user / LaunchAgent)。Windows の
    /// SCM にユーザースコープは無いので、その場合は無視される。
    service_user_scope: bool,

    // 実行結果
    log_lines: Vec<String>,
    setup_error: Option<String>,
    /// セットアップ本体のワーカースレッド。
    setup_rx: Option<mpsc::Receiver<SetupEvent>>,

    // 完了画面
    launch_deadline: Option<Instant>,
    launch_message: Option<String>,

    // 完了画面: 既存クライアントDLLの一括更新
    bulk_update_log: Vec<String>,
    bulk_update_error: Option<String>,
    bulk_update_ran: bool,
}

impl SetupApp {
    fn new() -> Self {
        Self {
            step: Step::ModeSelect,
            mode: SetupMode::FullAuto,
            listen_addr: "0.0.0.0:40070".to_string(),
            web_listen_addr: "0.0.0.0:40080".to_string(),
            lan_access: true,
            install_location: default_install_location(),
            bulk_update_dir: String::new(),
            dll_source_path: String::new(),
            detect_rx: None,
            detected: Vec::new(),
            selected: Vec::new(),
            manual_form: ManualEntryForm::default(),
            manual_entries: Vec::new(),
            installing_index: None,
            install_rx: None,
            install_log: Vec::new(),
            install_error: None,
            driver_install_failed: Vec::new(),
            overwrite_config: false,
            recreate_db: false,
            register_service: true,
            setup_preview: true,
            setup_tsreplace: false,
            tsreplace_quality: TsreplaceQuality::Compatibility,
            setup_4k: false,
            fourk_paths: Vec::new(),
            fourk_selected: Vec::new(),
            fourk_acas: AcasSelection::Automatic,
            fourk_reader_candidates: Vec::new(),
            fourk_probe_rx: None,
            firewall_allow: true,
            mirakurun_enabled: false,
            home_region: String::new(),
            node_display_name: default_node_display_name(),
            service_name: recisdb_proxy::service::DEFAULT_SERVICE_NAME.to_string(),
            service_registered: false,
            service_user_scope: false,
            log_lines: Vec::new(),
            setup_error: None,
            setup_rx: None,
            launch_deadline: None,
            launch_message: None,
            bulk_update_log: Vec::new(),
            bulk_update_error: None,
            bulk_update_ran: false,
        }
    }

    /// 入口でモードを選んだときの初期化。モードごとに既定値を変える。
    fn choose_mode(&mut self, mode: SetupMode) {
        self.mode = mode;
        match mode {
            SetupMode::FullAuto => {
                // 全部お任せ。プレビューもサービスも用意する。
                self.setup_preview = true;
                self.setup_tsreplace = false;
                self.register_service = true;
                self.step = Step::Location;
            }
            SetupMode::Manual => {
                // 自分で決めたい人向け。ダウンロードを伴うプレビュー準備は
                // 既定でOFFにし、必要なら確認画面で明示的に選んでもらう。
                self.setup_preview = false;
                self.setup_tsreplace = false;
                self.register_service = true;
                self.step = Step::Location;
            }
            SetupMode::DllOnly => {
                if self.dll_source_path.trim().is_empty() {
                    // このツールと同じフォルダに配布用DLLがあれば初期値にする。
                    if let Some(dll) = setup_exe_dir()
                        .map(|d| d.join("BonDriver_NetworkProxy.dll"))
                        .filter(|p| p.exists())
                    {
                        self.dll_source_path = dll.to_string_lossy().to_string();
                    }
                }
                self.step = Step::DllOnly;
            }
        }
    }

    /// 全自動モードかどうか (ドライバ導入を自動で進めてよいか)。
    fn is_full_auto(&self) -> bool {
        self.mode == SetupMode::FullAuto
    }

    fn set_access_scope(&mut self, lan: bool) {
        self.lan_access = lan;
        let host = if lan { "0.0.0.0" } else { "127.0.0.1" };
        self.listen_addr = replace_address_host(&self.listen_addr, host);
        self.web_listen_addr = replace_address_host(&self.web_listen_addr, host);
    }

    fn setup_config(&self) -> setup_helpers::SetupConfig {
        setup_helpers::SetupConfig {
            listen_addr: self.listen_addr.clone(),
            web_listen_addr: self.web_listen_addr.clone(),
            db_path: self.db_file_path().to_string_lossy().into_owned(),
            mirakurun_enabled: self.mirakurun_enabled,
            mirakurun_home_region: (!self.home_region.trim().is_empty())
                .then(|| self.home_region.trim().to_owned()),
            node_display_name: (!self.node_display_name.trim().is_empty())
                .then(|| self.node_display_name.trim().to_owned()),
            ..Default::default()
        }
    }

    fn refresh_fourk_candidates(&mut self) {
        let mut paths = self
            .detected
            .iter()
            .flat_map(|tuner| tuner.device_paths.iter())
            .chain(self.manual_entries.iter().map(|entry| &entry.path))
            .filter(|path| path.to_ascii_lowercase().ends_with(".dll"))
            .cloned()
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        let old = self
            .fourk_paths
            .iter()
            .zip(self.fourk_selected.iter())
            .filter_map(|(path, selected)| selected.then_some(path.clone()))
            .collect::<std::collections::HashSet<_>>();
        self.fourk_selected = paths.iter().map(|path| old.contains(path)).collect();
        self.fourk_paths = paths;
    }

    fn make_setup_job(&self) -> SetupJob {
        SetupJob {
            install_dir: self.install_dir(),
            source_dir: setup_exe_dir(),
            config_path: self.config_file_path(),
            db_path: self.db_file_path(),
            setup_config: self.setup_config(),
            overwrite_config: self.overwrite_config,
            recreate_db: self.recreate_db,
            detected: self.detected.clone(),
            selected: self.selected.clone(),
            manual_entries: self.manual_entries.clone(),
            setup_preview: self.setup_preview,
            setup_tsreplace: self.setup_tsreplace,
            tsreplace_quality: self.tsreplace_quality,
            setup_4k: self.setup_4k,
            fourk_paths: self
                .fourk_paths
                .iter()
                .zip(self.fourk_selected.iter())
                .filter_map(|(path, selected)| selected.then(|| PathBuf::from(path)))
                .collect(),
            fourk_acas: self.fourk_acas.clone(),
            listen_addr: self.listen_addr.clone(),
            web_listen_addr: self.web_listen_addr.clone(),
            lan_access: self.lan_access,
            firewall_allow: self.firewall_allow,
            register_service: self.register_service,
            service_name: self.service_name.clone(),
            service_user_scope: self.service_user_scope,
        }
    }

    fn apply_existing_options(&self, mut content: String) -> String {
        // 既存ファイルではウィザードで選んだ詳細設定だけを反映し、listen と
        // database.path は既存値を優先する。
        content = config_file::upsert_key(
            &content,
            "server",
            "web_listen",
            &TomlValue::Str(self.web_listen_addr.clone()),
        );
        content = config_file::upsert_key(
            &content,
            "mirakurun",
            "enabled",
            &TomlValue::Bool(self.mirakurun_enabled),
        );
        content = if self.home_region.trim().is_empty() {
            config_file::remove_key(&content, "mirakurun", "home_region")
        } else {
            config_file::upsert_key(
                &content,
                "mirakurun",
                "home_region",
                &TomlValue::Str(self.home_region.trim().to_owned()),
            )
        };
        content = if self.node_display_name.trim().is_empty() {
            config_file::remove_key(&content, "node", "display_name")
        } else {
            config_file::upsert_key(
                &content,
                "node",
                "display_name",
                &TomlValue::Str(self.node_display_name.trim().to_owned()),
            )
        };
        content
    }

    fn start_detection(&mut self) {
        let install_dir = self.install_dir();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = setup_helpers::detect_tuners(&install_dir);
            let _ = tx.send(result);
        });
        self.detect_rx = Some(rx);
        self.step = Step::Detecting;
    }

    /// recisdb-proxy 本体・設定ファイル・データベースを配置するフォルダ
    /// (絶対パス)。
    ///
    /// 絶対パスにしておく理由: px4_drv のドライバインストールは別プロセスを
    /// UAC昇格して実行するが、昇格したプロセスの作業ディレクトリは
    /// (呼び出し元と異なり) 既定で C:\Windows\System32 になる。相対パスの
    /// ままだとそこを起点に解決されてファイルが見つからなくなるため。
    /// `canonicalize` は `\\?\` UNC プレフィックス付きパスを返し一部の
    /// ツールと相性が悪いことがあるため使わない。
    fn install_dir(&self) -> PathBuf {
        let dir = PathBuf::from(&self.install_location);
        if dir.is_absolute() {
            dir
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&dir))
                .unwrap_or(dir)
        }
    }

    fn config_file_path(&self) -> PathBuf {
        self.install_dir().join("recisdb-proxy.toml")
    }

    fn db_file_path(&self) -> PathBuf {
        self.install_dir().join("recisdb-proxy.db")
    }

    /// 一括更新の元にするDLLを決める。
    ///
    /// 1. 画面で明示指定されていればそれ (DLL差し替えモード)
    /// 2. インストール先のクライアント配布フォルダにあるもの (セットアップ直後)
    /// 3. このツール自身の隣にあるもの (リリースzipを展開しただけの状態)
    fn resolve_source_dll(&self) -> PathBuf {
        let explicit = self.dll_source_path.trim();
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
        let bundled = self
            .install_dir()
            .join(setup_helpers::CLIENT_CONFIG_DIR)
            .join("BonDriver_NetworkProxy.dll");
        if bundled.exists() {
            return bundled;
        }
        setup_exe_dir()
            .map(|d| d.join("BonDriver_NetworkProxy.dll"))
            .unwrap_or(bundled)
    }

    /// 「今すぐ一括更新を実行する」ボタンを押したときの処理。
    /// [`SetupApp::resolve_source_dll`] が返すDLLを元に、`self.bulk_update_dir`
    /// 以下 (サブフォルダ含む) の `BonDriver_NetworkProxy` 接頭辞DLLをまとめて
    /// 上書きする。インストール先フォルダとは無関係に、任意のフォルダ
    /// (例: TVTestのBonDriverフォルダ) を対象にできる。
    fn run_bulk_dll_update(&mut self) {
        self.bulk_update_log.clear();
        self.bulk_update_error = None;
        self.bulk_update_ran = true;

        let target_dir = self.bulk_update_dir.trim();
        if target_dir.is_empty() {
            self.bulk_update_error = Some("更新先フォルダを指定してください。".to_string());
            return;
        }

        let source_dll = self.resolve_source_dll();

        match bulk_update_bondriver_dlls(&source_dll, Path::new(target_dir)) {
            Ok(log) => self.bulk_update_log = log,
            Err(e) => self.bulk_update_error = Some(e),
        }
    }

    /// 指定したチューナーの px4_drv ドライバ自動インストールをバックグラウンドで開始する。
    fn start_px4_install(&mut self, index: usize) {
        let Some(pid) = self.detected[index].px4_model_pid else {
            return;
        };
        let install_dir = self.install_dir();

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let result = px4_installer::download_install_and_stage(pid, &install_dir, move |msg| {
                let _ = progress_tx.send(InstallEvent::Progress(msg.to_string()));
            });
            let _ = tx.send(InstallEvent::Done(result));
        });

        self.installing_index = Some(index);
        self.install_rx = Some(rx);
        self.install_log.clear();
        self.install_error = None;
    }

    /// ドライバが未導入で、まだ自動インストールを試していないチューナーの添字。
    fn next_driver_install_target(&self) -> Option<usize> {
        self.detected.iter().enumerate().position(|(i, t)| {
            t.px4_model_pid.is_some()
                && t.device_paths.is_empty()
                && !self.driver_install_failed.contains(&i)
        })
    }

    /// 全自動モードで、ドライバ未導入のチューナーを1台ずつ順に処理する。
    fn start_next_driver_install_if_auto(&mut self) {
        if !self.is_full_auto() || self.installing_index.is_some() {
            return;
        }
        if let Some(i) = self.next_driver_install_target() {
            self.start_px4_install(i);
        }
    }

    /// px4_drv インストールスレッドからの通知を受け取り、状態を更新する。
    fn poll_px4_install(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.install_rx else {
            return;
        };

        loop {
            match rx.try_recv() {
                Ok(InstallEvent::Progress(msg)) => self.install_log.push(msg),
                Ok(InstallEvent::Done(result)) => {
                    let idx = self
                        .installing_index
                        .take()
                        .expect("installing_index set while install_rx is Some");
                    match result {
                        Ok(paths) => {
                            self.detected[idx].device_paths = paths;
                            self.selected[idx] = true;
                            self.install_error = None;
                        }
                        Err(e) => {
                            self.driver_install_failed.push(idx);
                            self.install_error = Some(e);
                        }
                    }
                    self.install_rx = None;
                    // 全自動モードでは次の未導入チューナーへ自動で進む。
                    self.start_next_driver_install_if_auto();
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(150));
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(idx) = self.installing_index.take() {
                        self.driver_install_failed.push(idx);
                    }
                    self.install_rx = None;
                    self.install_error =
                        Some("インストール処理が予期せず終了しました。".to_string());
                    self.start_next_driver_install_if_auto();
                    break;
                }
            }
        }
    }

    fn poll_setup(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.setup_rx else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(SetupEvent::Progress(message)) => self.log_lines.push(message),
                Ok(SetupEvent::SmartCardReaders(readers)) => {
                    self.fourk_reader_candidates = readers;
                }
                Ok(SetupEvent::Done(result)) => {
                    self.setup_rx = None;
                    match result {
                        Ok(result) => {
                            self.log_lines = result.log_lines;
                            self.service_registered = result.service_registered;
                            self.step = Step::Done;
                        }
                        Err(error) => self.setup_error = Some(error),
                    }
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(100));
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.setup_rx = None;
                    self.setup_error = Some("セットアップ処理が予期せず終了しました。".to_owned());
                    break;
                }
            }
        }
    }

    fn start_fourk_probe(&mut self) {
        if self.fourk_probe_rx.is_some() {
            return;
        }
        let install_dir = self.install_dir();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = {
                let mut ignored_progress = |_message: &str| {};
                fourk_setup::ensure_dantto_package_for_setup(&install_dir, &mut ignored_progress)
                    .and_then(|_| {
                        let exe =
                            fourk_setup::find_dantto_executable(&install_dir).ok_or_else(|| {
                                "展開後にdantto4k.exeが見つかりませんでした".to_owned()
                            })?;
                        fourk_setup::list_smart_card_readers(&exe)
                    })
            };
            let _ = tx.send(result);
        });
        self.fourk_probe_rx = Some(rx);
    }

    fn poll_fourk_probe(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.fourk_probe_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.fourk_probe_rx = None;
                match result {
                    Ok(readers) => self.fourk_reader_candidates = readers,
                    Err(error) => self.log_lines.push(format!(
                        "カードリーダー候補を取得できませんでした。手入力できます: {error}"
                    )),
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.fourk_probe_rx = None;
            }
        }
    }

    fn run_setup(&mut self) {
        if self.setup_rx.is_some() {
            return;
        }
        self.log_lines.clear();
        self.setup_error = None;
        let job = self.make_setup_job();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let reader_tx = tx.clone();
            let result = job.run(
                |message| {
                    let _ = progress_tx.send(SetupEvent::Progress(message.to_owned()));
                },
                |readers| {
                    let _ = reader_tx.send(SetupEvent::SmartCardReaders(readers));
                },
            );
            let _ = tx.send(SetupEvent::Done(result));
        });
        self.setup_rx = Some(rx);
    }

    fn launch_server_and_open_dashboard(&mut self) {
        let install_dir = self.install_dir();
        let exe_name = if cfg!(windows) {
            "recisdb-proxy.exe"
        } else {
            "recisdb-proxy"
        };
        let exe = install_dir.join(exe_name);

        if !exe.exists() {
            self.launch_message = Some(format!(
                "{} が見つかりませんでした。セットアップを実行してから、もう一度お試しください。",
                exe.display()
            ));
            return;
        }

        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--config").arg(self.config_file_path());
        cmd.current_dir(&install_dir);

        match cmd.spawn() {
            Ok(_) => {
                self.launch_message = Some(
                    "recisdb-proxy を起動しました。数秒後にダッシュボードを開きます…".to_string(),
                );
                self.launch_deadline = Some(Instant::now() + Duration::from_secs(2));
            }
            Err(e) => {
                self.launch_message = Some(format!("recisdb-proxy の起動に失敗しました: {e}"));
            }
        }
    }
}

impl SetupJob {
    fn run(
        mut self,
        progress: impl FnMut(&str),
        smart_card_readers: impl FnMut(Vec<String>),
    ) -> Result<SetupResult, String> {
        #[cfg(windows)]
        let mut smart_card_readers = smart_card_readers;
        #[cfg(not(windows))]
        let _ = &smart_card_readers;
        std::fs::create_dir_all(&self.install_dir)
            .map_err(|e| format!("インストール先フォルダの作成に失敗しました: {e}"))?;
        let logs = std::cell::RefCell::new(Vec::new());
        let progress = std::cell::RefCell::new(progress);
        let mut emit = |message: String| {
            progress.borrow_mut()(&message);
            logs.borrow_mut().push(message);
        };

        let source_dir = self
            .source_dir
            .as_deref()
            .ok_or_else(|| "実行ファイルの場所を取得できませんでした。".to_owned())?;
        match setup_helpers::sync_program_binary(source_dir, &self.install_dir)? {
            setup_helpers::BinarySyncAction::FreshInstall => emit(format!(
                "recisdb-proxyをインストールしました: {}",
                self.install_dir.display()
            )),
            setup_helpers::BinarySyncAction::Updated => {
                emit("recisdb-proxyを最新版に更新しました。".to_owned())
            }
            setup_helpers::BinarySyncAction::AlreadyUpToDate => {
                emit("recisdb-proxyは既に最新の状態です。".to_owned())
            }
        }

        if self.setup_tsreplace {
            emit("tsreplaceを準備しています…".to_owned());
            let result = fourk_setup::ensure_tsreplace(&self.install_dir, &mut |message: &str| {
                progress.borrow_mut()(message);
                logs.borrow_mut().push(message.to_owned());
            });
            match result {
                Ok(path) => {
                    self.setup_config.tsreplace_command_path =
                        Some(path.to_string_lossy().into_owned());
                    match recisdb_proxy::preview_setup::resolve_tsreadex_ready(&self.install_dir) {
                        Ok(tsreadex) => {
                            self.setup_config.tsreplace_preprocessor_path =
                                Some(tsreadex.to_string_lossy().into_owned());
                            emit(format!(
                                "tsreplaceとtsreadexを設定します: {} / {}",
                                path.display(),
                                tsreadex.display()
                            ));
                        }
                        Err(error) => {
                            self.setup_config.tsreplace_preprocessor_path = Some(String::new());
                            emit(format!("tsreplaceは見つかりましたがtsreadexの準備に失敗しました。前段なしで設定します: {error}"));
                        }
                    }
                }
                Err(error) => emit(format!(
                    "tsreplaceの準備に失敗しました。設定には書きません: {error}"
                )),
            }
        }

        let config_content = if !self.config_path.exists() || self.overwrite_config {
            generate_config(&self.setup_config)
        } else {
            let content = std::fs::read_to_string(&self.config_path)
                .map_err(|e| format!("既存の設定ファイルの読み込みに失敗しました: {e}"))?;
            apply_existing_options_job(&self, content)
        };
        config_file::validate(&config_content)
            .map_err(|error| format!("設定ファイルが不正なTOMLです。保存しません: {error}"))?;
        std::fs::write(&self.config_path, config_content)
            .map_err(|e| format!("設定ファイルの保存に失敗しました: {e}"))?;
        emit(format!(
            "設定ファイルを保存しました: {}",
            self.config_path.display()
        ));

        configure_firewall_job(&self, &mut emit);

        if self.db_path.exists() && self.recreate_db {
            let backup = format!("{}.backup", self.db_path.display());
            std::fs::rename(&self.db_path, &backup)
                .map_err(|e| format!("データベースのバックアップに失敗しました: {e}"))?;
            emit(format!(
                "既存のデータベースをバックアップしました: {backup}"
            ));
        }
        let db = Database::open(&self.db_path)
            .map_err(|e| format!("データベースの初期化に失敗しました: {e}"))?;
        emit(format!(
            "データベースを初期化しました: {}",
            self.db_path.display()
        ));

        if let Some(command_path) = self.setup_config.tsreplace_command_path.as_deref() {
            db.set_tsreplace_command_path(command_path)
                .map_err(|error| format!("tsreplaceの実行ファイルパス保存に失敗しました: {error}"))?;
            let preprocessor_path = self
                .setup_config
                .tsreplace_preprocessor_path
                .as_deref()
                .unwrap_or_default();
            db.set_tsreplace_preprocessor_path(preprocessor_path).map_err(|error| {
                format!("tsreplace前処理の実行ファイルパス保存に失敗しました: {error}")
            })?;
        }

        if self.setup_tsreplace {
            if let Some(tsreplace_path) = self.setup_config.tsreplace_command_path.as_deref() {
                emit("tsreplaceのエンコード設定を検出しています…".to_owned());
                match encoder_probe::select_tsreplace_arguments(
                    &self.install_dir,
                    Path::new(tsreplace_path),
                    self.tsreplace_quality,
                ) {
                    Ok(selection) => {
                        let current = db
                            .get_tsreplace_config()
                            .map_err(|error| format!("tsreplace設定の読み込みに失敗しました: {error}"))?;
                        if encoder_probe::tsreplace_arguments_is_auto_generated(Some(&current.2)) {
                            db.update_tsreplace_config(
                                current.0,
                                &current.1,
                                &selection.arguments,
                                current.3,
                                current.4,
                                current.5,
                                &current.6,
                                &current.7,
                            )
                            .map_err(|error| format!("tsreplace引数の保存に失敗しました: {error}"))?;
                            emit(format!(
                                "tsreplace設定を自動更新しました: エンコーダ={} / コーデック={} / 理由={}",
                                selection.encoder, selection.codec, selection.reason
                            ));
                        } else {
                            emit("tsreplace引数は管理者が編集済みのため上書きしませんでした。enabledはダッシュボードで変更してください。".to_owned());
                        }
                        for warning in selection.warnings {
                            emit(format!("tsreplace候補を不採用: {warning}"));
                        }
                    }
                    Err(error) => emit(format!(
                        "tsreplaceの自動設定に失敗しました。現在の引数とenabledは変更しません: {error}"
                    )),
                }
            } else {
                emit("tsreplace本体が準備できなかったため、エンコード設定を変更しませんでした。".to_owned());
            }
        }

        let fourk_set: std::collections::HashSet<String> = self
            .fourk_paths
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect();
        let mut normal_detected = self.detected.clone();
        for tuner in &mut normal_detected {
            let removed = tuner
                .device_paths
                .iter()
                .filter(|path| fourk_set.contains(*path))
                .count();
            if removed > 0 {
                emit(format!(
                    "4K選択済みの基底BonDriverを通常チューナー登録から除外しました: {}件",
                    removed
                ));
            }
            tuner.device_paths.retain(|path| !fourk_set.contains(path));
        }
        let selected_indices: Vec<usize> = self
            .selected
            .iter()
            .enumerate()
            .filter_map(|(index, selected)| selected.then_some(index))
            .collect();
        for result in register_tuners_to_db(&db, &normal_detected, &selected_indices) {
            match result.outcome {
                Ok(id) => emit(format!(
                    "チューナーを登録しました: {} (ID: {id})",
                    result.device_path
                )),
                Err(error) => emit(format!(
                    "チューナーの登録に失敗しました: {} ({error})",
                    result.device_path
                )),
            }
        }

        for entry in &self.manual_entries {
            if fourk_set.contains(&entry.path) {
                emit(format!(
                    "4K選択済みの手動BonDriverを通常登録から除外しました: {}",
                    entry.path
                ));
                continue;
            }
            match register_manual_tuner(&db, &entry.path, &entry.group, entry.max_instances) {
                Ok(id) => emit(format!(
                    "チューナーを登録しました: {} (ID: {id})",
                    entry.path
                )),
                Err(error) => emit(format!(
                    "チューナーの登録に失敗しました: {} ({error})",
                    entry.path
                )),
            }
        }

        if self.setup_4k && !self.fourk_paths.is_empty() {
            emit("dantto4kラッパーを準備しています…".to_owned());
            let wrappers_result = fourk_setup::prepare_wrappers(
                &self.install_dir,
                &self.fourk_paths,
                &self.fourk_acas,
                &mut |message: &str| {
                    progress.borrow_mut()(message);
                    logs.borrow_mut().push(message.to_owned());
                },
            );
            let wrappers = wrappers_result?;
            #[cfg(windows)]
            if let Some(exe) = fourk_setup::find_dantto_executable(&self.install_dir) {
                match fourk_setup::list_smart_card_readers(&exe) {
                    Ok(readers) => smart_card_readers(readers),
                    Err(error) => emit(format!(
                        "カードリーダー候補の取得に失敗しました。手入力できます: {error}"
                    )),
                }
            }
            for wrapper in wrappers {
                // 検出済みチューナーの本数(地デジ+衛星)はユニット全体の値で、
                // ラッパー1個(=基底DLL1個)の同時オープン数ではない。
                // 手動追加で明示された場合だけその値を使い、他は1とする。
                let max_instances = self
                    .manual_entries
                    .iter()
                    .find(|entry| entry.path == wrapper.base_path.to_string_lossy())
                    .map(|entry| entry.max_instances.max(1))
                    .unwrap_or(1);
                let wrapper_path = wrapper.wrapper_path.to_string_lossy().to_string();
                match register_manual_tuner(&db, &wrapper_path, "BS4K", max_instances) {
                    Ok(id) => {
                        db.set_driver_stream_format(&wrapper_path, StreamFormat::Ts)
                            .map_err(|e| format!("4Kストリーム形式の保存に失敗しました: {e}"))?;
                        db.set_driver_disable_b25(&wrapper_path, true)
                            .map_err(|e| format!("4K B25無効化の保存に失敗しました: {e}"))?;
                        emit(format!("BS4Kラッパーを登録しました: {} (ID: {id}, stream_format=ts, disable_b25=true)", wrapper_path));
                    }
                    Err(error) => emit(format!(
                        "BS4Kラッパーの登録に失敗しました: {wrapper_path} ({error})"
                    )),
                }
            }
            emit("4Kチャンネルはスキャン後に番組表/チャンネル一覧の末尾(BS4K)に追加されます。TVTestの.ch2は再生成してください。".to_owned());
        }

        let tuner_hint = db
            .get_all_bon_drivers()
            .ok()
            .and_then(|drivers| {
                drivers
                    .iter()
                    .find_map(|driver| {
                        driver
                            .group_name
                            .clone()
                            .filter(|group| !group.trim().is_empty())
                    })
                    .or_else(|| drivers.first().map(|driver| driver.dll_path.clone()))
            })
            .unwrap_or_default();
        let proxy_port = self.listen_addr.rsplit(':').next().unwrap_or("40070");
        let web_port = self.web_listen_addr.rsplit(':').next().unwrap_or("40080");
        let ip = setup_helpers::local_lan_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".to_owned());
        if let Err(error) = setup_helpers::write_client_config_bundle(
            &self.install_dir,
            self.source_dir.as_deref(),
            &format!("{ip}:{proxy_port}"),
            &tuner_hint,
            &format!("http://{ip}:{web_port}"),
        ) {
            emit(format!("クライアント設定の出力に失敗しました: {error}"));
        }

        if self.setup_preview {
            emit("ブラウザプレビューを準備しています…".to_owned());
            let result = recisdb_proxy::preview_setup::ensure_preview_ready(
                &db,
                &self.install_dir,
                Some(&self.config_path),
            );
            match result {
                Ok(report) => {
                    emit(format!(
                        "ブラウザプレビューを有効にしました (エンコーダ: {} / 映像: {})",
                        report.encoder_path, report.video_encoder
                    ));
                    if report.preprocessor_path.is_empty() {
                        emit(
                            "前段処理(tsreadex)は未設定です。字幕が表示されない場合があります。"
                                .to_owned(),
                        );
                    }
                    for warning in report.warnings {
                        emit(warning);
                    }
                }
                Err(error) => emit(format!(
                    "ブラウザプレビューの準備に失敗しました (視聴・録画には影響しません): {error}"
                )),
            }
        }

        let service_registered = if self.register_service && recisdb_proxy::service::is_supported()
        {
            register_os_service_job(&self, &mut emit)
        } else {
            false
        };
        let _ = emit;
        Ok(SetupResult {
            log_lines: logs.into_inner(),
            service_registered,
        })
    }
}

fn apply_existing_options_job(job: &SetupJob, mut content: String) -> String {
    content = config_file::upsert_key(
        &content,
        "server",
        "web_listen",
        &TomlValue::Str(job.web_listen_addr.clone()),
    );
    content = config_file::upsert_key(
        &content,
        "mirakurun",
        "enabled",
        &TomlValue::Bool(job.setup_config.mirakurun_enabled),
    );
    content = match &job.setup_config.mirakurun_home_region {
        Some(value) => config_file::upsert_key(
            &content,
            "mirakurun",
            "home_region",
            &TomlValue::Str(value.clone()),
        ),
        None => config_file::remove_key(&content, "mirakurun", "home_region"),
    };
    content = match &job.setup_config.node_display_name {
        Some(value) => config_file::upsert_key(
            &content,
            "node",
            "display_name",
            &TomlValue::Str(value.clone()),
        ),
        None => config_file::remove_key(&content, "node", "display_name"),
    };
    if let Some(path) = &job.setup_config.tsreplace_command_path {
        content = config_file::upsert_key(
            &content,
            "tsreplace",
            "command_path",
            &TomlValue::Str(path.clone()),
        );
    }
    if let Some(path) = &job.setup_config.tsreplace_preprocessor_path {
        content = config_file::upsert_key(
            &content,
            "tsreplace",
            "preprocessor_path",
            &TomlValue::Str(path.clone()),
        );
    }
    content
}

fn configure_firewall_job(job: &SetupJob, log: &mut impl FnMut(String)) {
    if !job.lan_access {
        return;
    }
    let proxy_port = job.listen_addr.rsplit(':').next().unwrap_or("40070");
    let node_port = proxy_port
        .parse::<u16>()
        .ok()
        .and_then(|port| port.checked_add(1))
        .map(|port| port.to_string())
        .unwrap_or_else(|| "40071".to_owned());
    let web_port = job.web_listen_addr.rsplit(':').next().unwrap_or("40080");
    if cfg!(windows) {
        if !job.firewall_allow {
            log(format!("Windowsファイアウォールの自動設定をスキップしました。必要なら{proxy_port}/{node_port}/{web_port}を許可してください。"));
            return;
        }
        let exe = job.install_dir.join("recisdb-proxy.exe");
        let _ = std::process::Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                "name=recisdb-proxy",
            ])
            .status();
        let result = std::process::Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "add",
                "rule",
                "name=recisdb-proxy",
                "dir=in",
                "action=allow",
                &format!("program={}", exe.display()),
                "enable=yes",
                "profile=private,domain",
            ])
            .status();
        if matches!(result, Ok(status) if status.success()) {
            log("WindowsファイアウォールにPrivate/Domain用の受信許可ルールを追加しました。Publicは開けていません。".to_owned());
        } else {
            log(format!(
                "Windowsファイアウォールの設定に失敗しました。管理者権限で手動設定してください: {}",
                exe.display()
            ));
        }
    } else {
        log(format!("ファイアウォールを使っている場合はポート{proxy_port}/{node_port}/{web_port}を許可してください。"));
    }
}

fn register_os_service_job(job: &SetupJob, log: &mut impl FnMut(String)) -> bool {
    use recisdb_proxy::service::{self, ServiceScope};
    let name = match service::sanitize_service_name(&job.service_name) {
        Ok(name) => name,
        Err(error) => {
            log(format!(
                "サービス名が不正なため登録をスキップしました: {error}"
            ));
            return false;
        }
    };
    let scope = if job.service_user_scope && !cfg!(windows) {
        ServiceScope::User
    } else {
        ServiceScope::System
    };
    let exe_name = if cfg!(windows) {
        "recisdb-proxy.exe"
    } else {
        "recisdb-proxy"
    };
    let exe_path = job.install_dir.join(exe_name);
    let spec = service::default_spec(
        name.clone(),
        scope,
        exe_path.clone(),
        job.install_dir.clone(),
        vec![
            "-f".to_owned(),
            job.config_path.to_string_lossy().into_owned(),
        ],
    );
    match service::install(&spec) {
        Ok(()) => {
            log(format!("サービス`{name}`を登録し、開始しました。"));
            true
        }
        Err(error) => {
            log(format!("サービスの登録に失敗しました: {error}"));
            log(if cfg!(windows) {
                format!(
                    "管理者として`\"{}\" service install --name {name}`を実行してください。",
                    exe_path.display()
                )
            } else {
                format!(
                    "`sudo \"{}\" service install --name {name}`を実行してください。",
                    exe_path.display()
                )
            });
            false
        }
    }
}

/// 現在実行中のこのツール自身が置かれているフォルダ。recisdb-proxy 本体を
/// インストール先へコピーしてくる際のコピー元として使う(ダウンロードした
/// リリースzipの展開先を想定)。
fn setup_exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(Path::to_path_buf)
}

/// OS既定のブラウザでURLを開く。失敗しても致命的ではないので握りつぶす。
fn open_in_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

/// `0.0.0.0:40080` のような待ち受けアドレスを、ブラウザで開ける
/// `http://localhost:40080` に変換する。
fn dashboard_url(web_listen_addr: &str) -> String {
    let port = web_listen_addr.rsplit(':').next().unwrap_or("40080");
    format!("http://localhost:{port}")
}

fn replace_address_host(address: &str, host: &str) -> String {
    address
        .rsplit_once(':')
        .map(|(_, port)| format!("{host}:{port}"))
        .unwrap_or_else(|| format!("{host}:{address}"))
}

fn prefecture_names() -> Vec<String> {
    let mut names = Vec::new();
    for region_id in 1..=62 {
        if let Some(name) =
            recisdb_protocol::broadcast_region::get_prefecture_name_from_region_id(region_id)
        {
            if recisdb_protocol::broadcast_region::region_ids_from_prefecture_name(name).is_empty()
            {
                continue;
            }
            if !names.iter().any(|known| known == name) {
                names.push(name.to_owned());
            }
        }
    }
    names
}

impl eframe::App for SetupApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // チューナー検出の完了待ち
        if let Some(rx) = &self.detect_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.selected = vec![true; result.len()];
                    self.detected = result;
                    self.refresh_fourk_candidates();
                    self.detect_rx = None;
                    self.step = Step::SelectTuners;
                    // 全自動モードなら、ドライバ未導入のチューナーを続けて処理する。
                    self.start_next_driver_install_if_auto();
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.detect_rx = None;
                    self.detected = Vec::new();
                    self.step = Step::SelectTuners;
                }
            }
        }

        self.poll_px4_install(&ctx);
        self.poll_fourk_probe(&ctx);
        self.poll_setup(&ctx);

        // recisdb-proxy 起動後、少し待ってからダッシュボードを開く
        if let Some(deadline) = self.launch_deadline {
            if Instant::now() >= deadline {
                open_in_browser(&dashboard_url(&self.web_listen_addr));
                self.launch_deadline = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(palette::BG)
                    .inner_margin(egui::Margin::symmetric(28, 24)),
            )
            .show(ui, |ui| {
                // 文字を大きくしたぶん、どの画面もウィンドウ高を超えうる。
                // ページ全体をスクロール可能にして、入力欄がビューポート外に
                // 溢れて操作不能になるのを防ぐ。
                egui::ScrollArea::vertical()
                    .id_salt("page_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.step {
                        Step::ModeSelect => self.ui_mode_select(ui),
                        Step::Location => self.ui_location(ui),
                        Step::Detecting => self.ui_detecting(ui),
                        Step::SelectTuners => self.ui_select_tuners(ui),
                        Step::Options => self.ui_options(ui),
                        Step::Confirm => self.ui_confirm(ui),
                        Step::Done => self.ui_done(ui),
                        Step::DllOnly => self.ui_dll_only(ui),
                    });
            });
    }
}

impl SetupApp {
    /// 入口。3つの作業のうちどれをするのかを最初に選ばせる。
    fn ui_mode_select(&mut self, ui: &mut egui::Ui) {
        ui.heading("recisdb-proxy かんたんセットアップ");
        ui.add_space(6.0);
        ui.label("行いたい作業を選んでください。あとから何度でもやり直せます。");
        ui.add_space(16.0);

        let mut chosen: Option<SetupMode> = None;

        chosen = self
            .mode_card(
                ui,
                SetupMode::FullAuto,
                "おすすめ",
                "チューナーの検出からドライバの導入、ブラウザ視聴の準備、PC起動時の自動開始まで、\
                 すべて自動で行います。はじめて設定する場合はこちらを選んでください。",
                &[
                    "チューナーを自動で探して登録します",
                    "ドライバが未導入なら自動で入れます",
                    "ブラウザでの映像確認を使えるようにします",
                    "PC起動時に自動で動くよう登録します",
                ],
            )
            .or(chosen);

        ui.add_space(12.0);

        chosen = self
            .mode_card(
                ui,
                SetupMode::Manual,
                "手動で設定",
                "ドライバの自動インストールを行いません。すでにドライバを入れてある場合や、\
                 登録するチューナーを自分で指定したい場合はこちらを選んでください。",
                &[
                    "ドライバの導入は行いません (未導入なら案内のみ)",
                    "登録するチューナーを自分で選べます",
                    "チューナーを手入力で追加できます",
                    "プレビュー準備・サービス登録も個別に選べます",
                ],
            )
            .or(chosen);

        ui.add_space(12.0);

        chosen = self
            .mode_card(
                ui,
                SetupMode::DllOnly,
                "更新のみ",
                "TVTest/EDCB 側に配置済みの BonDriver_NetworkProxy*.dll を、新しい版に差し替えます。\
                 サーバー本体・設定ファイル・データベースには一切触れません。",
                &[
                    "指定フォルダ以下のDLLをまとめて上書きします",
                    "ファイル名 (別名で複製したもの) はそのまま保ちます",
                    "設定・データベースは変更しません",
                ],
            )
            .or(chosen);

        if let Some(mode) = chosen {
            self.choose_mode(mode);
        }

        ui.add_space(16.0);
        if secondary_button(ui, "終了").clicked() {
            std::process::exit(0);
        }
    }

    /// モード選択カード1枚。押されたらそのモードを返す。
    fn mode_card(
        &self,
        ui: &mut egui::Ui,
        mode: SetupMode,
        badge: &str,
        description: &str,
        bullets: &[&str],
    ) -> Option<SetupMode> {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(mode.title())
                        .size(21.0)
                        .color(palette::TEXT),
                );
                ui.add_space(6.0);
                egui::Frame::new()
                    .fill(palette::FAINT)
                    .corner_radius(6)
                    .inner_margin(egui::Margin::symmetric(8, 3))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(badge).size(14.0).color(palette::ACCENT));
                    });
            });
            ui.add_space(6.0);
            ui.label(description);
            ui.add_space(8.0);
            for b in bullets {
                ui.label(
                    egui::RichText::new(format!("・{b}"))
                        .size(15.0)
                        .color(palette::MUTED),
                );
            }
            ui.add_space(12.0);
            if primary_button(ui, "この作業を始める  ▶").clicked() {
                Some(mode)
            } else {
                None
            }
        })
    }

    /// DLL差し替え専用画面。本体インストールとは独立して単独で実行できる。
    fn ui_dll_only(&mut self, ui: &mut egui::Ui) {
        page_title(
            ui,
            &SetupMode::DllOnly.step_label(1),
            "クライアントDLLの差し替え",
        );
        ui.label(
            "TVTest/EDCB を動かすPCに配置してある BonDriver_NetworkProxy*.dll を、\
             新しい版の内容でまとめて上書きします。",
        );
        ui.add_space(16.0);

        card(ui, |ui| {
            ui.label(egui::RichText::new("更新元のDLL").size(19.0));
            hint(
                ui,
                "新しい版の BonDriver_NetworkProxy.dll を指定します。\
                 このツールと同じフォルダにあれば自動で入ります。",
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.dll_source_path)
                        .desired_width(480.0)
                        .hint_text("例: C:\\DTV\\recisdb-proxy-rs\\client-config\\BonDriver_NetworkProxy.dll"),
                );
                #[cfg(windows)]
                if ui.button("参照…").clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("DLL", &["dll"])
                        .pick_file()
                    {
                        self.dll_source_path = file.to_string_lossy().to_string();
                    }
                }
            });

            let source = self.resolve_source_dll();
            if source.exists() {
                ui.colored_label(palette::OK, format!("使用するDLL: {}", source.display()));
            } else {
                ui.colored_label(
                    palette::WARN,
                    format!("見つかりません: {}", source.display()),
                );
            }
        });

        ui.add_space(12.0);

        card(ui, |ui| {
            ui.label(egui::RichText::new("更新先フォルダ").size(19.0));
            hint(
                ui,
                "このフォルダ以下 (サブフォルダも含む) にある \"BonDriver_NetworkProxy\" で始まるDLLが\
                 すべて対象になります。別名で複製したファイル名はそのまま保たれます。",
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.bulk_update_dir)
                        .desired_width(480.0)
                        .hint_text("例: C:\\DTV\\TVTest\\BonDriver"),
                );
                #[cfg(windows)]
                if ui.button("参照…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.bulk_update_dir = dir.to_string_lossy().to_string();
                    }
                }
            });
        });

        ui.add_space(16.0);
        ui.horizontal(|ui| {
            if secondary_button(ui, "◀ 戻る").clicked() {
                self.step = Step::ModeSelect;
            }
            if primary_button(ui, "差し替えを実行する  ▶").clicked() {
                self.run_bulk_dll_update();
            }
        });

        if let Some(err) = &self.bulk_update_error {
            ui.add_space(12.0);
            error_box(ui, err);
        }

        if self.bulk_update_ran && self.bulk_update_error.is_none() {
            ui.add_space(12.0);
            card(ui, |ui| {
                if self.bulk_update_log.is_empty() {
                    ui.label("対象のDLLは見つかりませんでした。");
                } else {
                    ui.colored_label(palette::OK, "差し替えが完了しました。");
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .id_salt("dll_only_log_scroll")
                        .show(ui, |ui| {
                            for line in &self.bulk_update_log {
                                ui.label(line);
                            }
                        });
                }
            });
            ui.add_space(12.0);
            if secondary_button(ui, "終了").clicked() {
                std::process::exit(0);
            }
        }
    }

    fn ui_location(&mut self, ui: &mut egui::Ui) {
        page_title(ui, &self.mode.step_label(1), "インストール先の確認");
        ui.label(
            "recisdb-proxy 本体・設定ファイル・データベースを配置するフォルダです。\
             よくわからない場合はそのままで大丈夫です。",
        );
        ui.add_space(16.0);

        card(ui, |ui| {
            ui.label(egui::RichText::new("インストール先フォルダ").size(19.0));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.install_location).desired_width(480.0));
                #[cfg(windows)]
                if ui.button("参照…").clicked() {
                    let start_dir = self.install_dir();
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_directory(&start_dir)
                        .pick_folder()
                    {
                        self.install_location = dir.to_string_lossy().to_string();
                    }
                }
            });
            hint(
                ui,
                "既にインストール済みの場合は、本体プログラムだけが最新版に更新されます\
                 (設定・データベースはそのまま残ります)。",
            );
        });

        ui.add_space(12.0);

        card(ui, |ui| {
            ui.label(egui::RichText::new("クライアントDLLの一括更新 (省略可)").size(19.0));
            ui.add_space(4.0);
            hint(
                ui,
                "TVTest/EDCB を動かすPC側で、チューナーごとに別名で複製配置している既存の \
                 BonDriver_NetworkProxy 系DLL (例: BonDriver_NetworkProxy_1.dll) を、\
                 セットアップ完了後にまとめて最新版へ更新したい場合は、その置き場所を指定してください \
                 (サブフォルダも検索対象です)。空欄なら一括更新は行いません。",
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.bulk_update_dir)
                        .desired_width(480.0)
                        .hint_text("例: C:\\DTV\\TVTest\\BonDriver"),
                );
                #[cfg(windows)]
                if ui.button("参照…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.bulk_update_dir = dir.to_string_lossy().to_string();
                    }
                }
            });
        });

        ui.add_space(20.0);
        ui.horizontal(|ui| {
            if secondary_button(ui, "◀ 戻る").clicked() {
                self.step = Step::ModeSelect;
            }
            if primary_button(ui, "次へ  ▶").clicked() {
                self.start_detection();
            }
        });
    }

    fn ui_detecting(&mut self, ui: &mut egui::Ui) {
        page_title(ui, &self.mode.step_label(2), "チューナーを探しています…");
        ui.add_space(20.0);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.add_space(6.0);
                ui.label(
                    "接続されているチューナーを自動で検出しています。しばらくお待ちください。",
                );
            });
        });
    }

    fn ui_select_tuners(&mut self, ui: &mut egui::Ui) {
        page_title(ui, &self.mode.step_label(2), "使用するチューナーを選択");

        if self.detected.is_empty() {
            ui.label("チューナーは自動検出できませんでした。下から手動で追加できます。");
        } else {
            ui.label("見つかったチューナーのうち、使用するものにチェックを入れてください。");
            ui.add_space(12.0);

            let mut install_clicked: Option<usize> = None;
            let is_full_auto = self.is_full_auto();

            egui::ScrollArea::vertical()
                .max_height(320.0)
                .id_salt("tuner_list_scroll")
                .show(ui, |ui| {
                    for i in 0..self.detected.len() {
                        card(ui, |ui| {
                            let installing_this = self.installing_index == Some(i);
                            let needs_driver = self.detected[i].px4_model_pid.is_some()
                                && self.detected[i].device_paths.is_empty();

                            ui.horizontal(|ui| {
                                ui.checkbox(&mut self.selected[i], "");
                                ui.vertical(|ui| {
                                    let tuner = &self.detected[i];
                                    ui.label(egui::RichText::new(&tuner.name).size(19.0));
                                    if tuner.terrestrial_count > 0 || tuner.satellite_count > 0 {
                                        ui.label(format!(
                                            "地上波 {}ch / 衛星(BS/CS) {}ch",
                                            tuner.terrestrial_count, tuner.satellite_count
                                        ));
                                    }
                                    for path in &tuner.device_paths {
                                        ui.label(
                                            egui::RichText::new(path)
                                                .size(14.0)
                                                .color(palette::MUTED),
                                        );
                                    }

                                    if needs_driver {
                                        ui.add_space(4.0);
                                        if installing_this {
                                            ui.horizontal(|ui| {
                                                ui.spinner();
                                                let msg = self
                                                    .install_log
                                                    .last()
                                                    .cloned()
                                                    .unwrap_or_else(|| {
                                                        "インストール準備中…".to_string()
                                                    });
                                                ui.label(msg);
                                            });
                                        } else if is_full_auto {
                                            ui.horizontal(|ui| {
                                                ui.colored_label(
                                                    palette::WARN,
                                                    "ドライバが未インストールです",
                                                );
                                                let enabled = self.installing_index.is_none();
                                                if ui
                                                    .add_enabled(
                                                        enabled,
                                                        egui::Button::new(
                                                            "ドライバを自動インストール",
                                                        ),
                                                    )
                                                    .clicked()
                                                {
                                                    install_clicked = Some(i);
                                                }
                                            });
                                        } else {
                                            // 手動モードではドライバに触らない。
                                            // 何をすればよいかだけ伝える。
                                            ui.colored_label(
                                                palette::WARN,
                                                "ドライバが未インストールです \
                                                 (このモードでは自動導入を行いません。\
                                                 px4_drv を手動で導入するか、全自動モードで実行してください)",
                                            );
                                        }
                                    }
                                });
                            });
                        });
                        ui.add_space(8.0);
                    }
                });

            if let Some(i) = install_clicked {
                self.start_px4_install(i);
            }

            if let Some(err) = &self.install_error {
                ui.add_space(8.0);
                // pnputil等の詳細ログは複数行になりうるので、選択・コピーできる
                // スクロール可能なテキストボックスで表示する(単一行ラベルだと
                // 折り返しやコピーができず読みづらいため)。
                ui.colored_label(
                    palette::DANGER,
                    "ドライバのインストールでエラーが発生しました:",
                );
                let mut err_text = err.clone();
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .id_salt("install_error_scroll")
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut err_text)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .text_color(palette::DANGER),
                        );
                    });
            }
        }

        ui.add_space(12.0);
        ui.collapsing(
            egui::RichText::new("見つからない場合: 手動で追加する").size(17.0),
            |ui| {
                egui::Grid::new("manual_grid")
                    .num_columns(2)
                    .spacing([16.0, 10.0])
                    .show(ui, |ui| {
                        ui.label("チューナーのパス (DLLのパスまたはデバイスパス):");
                        ui.text_edit_singleline(&mut self.manual_form.path);
                        ui.end_row();

                        ui.label("グループ名 (省略可):");
                        ui.text_edit_singleline(&mut self.manual_form.group);
                        ui.end_row();

                        ui.label("最大同時使用数:");
                        ui.text_edit_singleline(&mut self.manual_form.max_instances);
                        ui.end_row();
                    });

                ui.add_space(6.0);
                if ui.button("この内容で追加").clicked() && !self.manual_form.path.trim().is_empty()
                {
                    let max_instances = self.manual_form.max_instances.trim().parse().unwrap_or(1);
                    self.manual_entries.push(ManualEntry {
                        path: self.manual_form.path.trim().to_string(),
                        group: self.manual_form.group.trim().to_string(),
                        max_instances,
                    });
                    self.refresh_fourk_candidates();
                    self.manual_form = ManualEntryForm::default();
                }

                if !self.manual_entries.is_empty() {
                    ui.add_space(8.0);
                    ui.label("追加予定のチューナー:");
                    let mut remove_at = None;
                    for (i, entry) in self.manual_entries.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(format!("  {} (グループ: {})", entry.path, entry.group));
                            if ui.button("削除").clicked() {
                                remove_at = Some(i);
                            }
                        });
                    }
                    if let Some(i) = remove_at {
                        self.manual_entries.remove(i);
                    }
                }
            },
        );

        ui.add_space(20.0);
        ui.horizontal(|ui| {
            if secondary_button(ui, "◀ 戻る").clicked() {
                self.step = Step::Location;
            }
            if primary_button(ui, "次へ  ▶").clicked() {
                self.overwrite_config = !self.config_file_path().exists();
                self.recreate_db = !self.db_file_path().exists();
                self.step = Step::Options;
            }
        });
    }

    fn ui_options(&mut self, ui: &mut egui::Ui) {
        page_title(ui, &self.mode.step_label(3), "詳細設定");
        hint(
            ui,
            "既定値のまま「次へ」を押せば安全に動作します。必要な項目だけ変更してください。",
        );
        ui.add_space(12.0);

        card(ui, |ui| {
            ui.label(egui::RichText::new("アクセス範囲").size(19.0));
            hint(
                ui,
                "LANを選ぶと、同じ家庭内のスマートフォンやTVTestから接続できます。認証は常に有効です。",
            );
            let old = self.lan_access;
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.lan_access, false, "このPCだけ");
                ui.radio_value(&mut self.lan_access, true, "家のネットワーク(LAN)から使う");
            });
            if old != self.lan_access {
                self.set_access_scope(self.lan_access);
            }
        });

        ui.add_space(10.0);
        if cfg!(windows) {
            card(ui, |ui| {
                ui.label(egui::RichText::new("Windowsファイアウォール").size(19.0));
                ui.add_enabled_ui(self.lan_access, |ui| {
                    ui.checkbox(&mut self.firewall_allow, "受信許可ルールを追加する");
                });
                hint(
                    ui,
                    "Private/Domainプロファイルだけを、recisdb-proxy.exe単位で許可します。Publicは開けません。",
                );
            });
        } else if self.lan_access {
            hint(
                ui,
                "ファイアウォールを使っている場合はポート 40070/40071/40080 を許可してください。",
            );
        }

        ui.add_space(10.0);
        card(ui, |ui| {
            ui.label(egui::RichText::new("録画ソフト連携 (Mirakurun互換API)").size(19.0));
            ui.checkbox(
                &mut self.mirakurun_enabled,
                "EPGStation など Mirakurun 対応ソフトから使う",
            );
            hint(
                ui,
                "既定では無効です。有効にすると /mirakurun/api/* が使えます。このAPIは認証なしのため、信頼できるネットワークだけで有効にしてください。",
            );
            if !self.lan_access && self.mirakurun_enabled {
                hint(
                    ui,
                    "「このPCだけ」の場合は、EPGStationも同じPCで動く場合だけ使えます。",
                );
            }
            let prefectures = prefecture_names();
            egui::ComboBox::from_id_salt("home_region")
                .selected_text(if self.home_region.is_empty() {
                    "指定しない"
                } else {
                    &self.home_region
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.home_region, String::new(), "指定しない");
                    for name in prefectures {
                        ui.selectable_value(&mut self.home_region, name.clone(), name);
                    }
                });
            hint(
                ui,
                "地元の都道府県を指定すると、他地域の地上波をNW1〜NW40に分けます。設定例には「大阪」を使います。",
            );
        });

        ui.add_space(10.0);
        card(ui, |ui| {
            ui.label(egui::RichText::new("分散ノード名").size(19.0));
            ui.text_edit_singleline(&mut self.node_display_name);
            hint(
                ui,
                "他のrecisdb-proxyから見える名前です。空欄なら自動生成名を使います。",
            );
        });

        ui.add_space(10.0);
        card(ui, |ui| {
            ui.label(egui::RichText::new("エンコーダ").size(19.0));
            ui.checkbox(
                &mut self.setup_preview,
                "ブラウザプレビューを使えるようにする",
            );
            hint(
                ui,
                "ffmpeg と tsreadex を検出または自動取得します。失敗してもTVTest視聴には影響しません。",
            );
            ui.checkbox(
                &mut self.setup_tsreplace,
                "TVTest向けエンコード(tsreplace)を用意する",
            );
            hint(
                ui,
                "tsreplaceとエンコーダを検出し、動作確認できた組み合わせを自動設定します。",
            );
            ui.add_enabled_ui(self.setup_tsreplace, |ui| {
                ui.label("画質方針");
                ui.radio_value(
                    &mut self.tsreplace_quality,
                    TsreplaceQuality::Compatibility,
                    "互換性重視 (H.264)",
                );
                ui.radio_value(
                    &mut self.tsreplace_quality,
                    TsreplaceQuality::Compression,
                    "圧縮率重視 (HEVC)",
                );
            });
        });

        if cfg!(windows) {
            ui.add_space(10.0);
            card(ui, |ui| {
                ui.label(egui::RichText::new("BS4Kチューナー (Windows)").size(19.0));
                ui.checkbox(&mut self.setup_4k, "BS4Kチューナーを使う");
                hint(
                    ui,
                    "4K放送を復号してTSへ変換するBonDriver_dantto4kラッパーを自動構成します。既定はOFFです。",
                );
                if self.setup_4k {
                    ui.add_space(6.0);
                    ui.label("4KチューナーのBonDriver (複数選択可)");
                    if self.fourk_paths.is_empty() {
                        hint(ui, "選択可能なDLLがありません。「チューナー選択」で検出または手動追加してください。");
                    }
                    for (index, path) in self.fourk_paths.iter().enumerate() {
                        ui.checkbox(&mut self.fourk_selected[index], path);
                    }
                    #[cfg(windows)]
                    if ui.button("ファイルを選ぶ").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("BonDriver DLL", &["dll"])
                            .pick_file()
                        {
                            let path = path.to_string_lossy().to_string();
                            if !self.fourk_paths.iter().any(|known| known == &path) {
                                self.fourk_paths.push(path);
                                self.fourk_selected.push(true);
                            }
                        }
                    }
                    ui.add_space(6.0);
                    ui.label("B-CAS/A-CASカードの読み方");
                    if self.fourk_probe_rx.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("dantto4kを準備してカードリーダー候補を取得しています…");
                        });
                    }
                    let automatic = matches!(self.fourk_acas, AcasSelection::Automatic);
                    if ui
                        .radio(automatic, "このPCのカードリーダーを使う (自動)")
                        .clicked()
                    {
                        self.fourk_acas = AcasSelection::Automatic;
                    }
                    hint(
                        ui,
                        "ACASは4K放送の復号に使うカードです。自動で利用可能なリーダーを探します。",
                    );
                    let reader_mode = matches!(self.fourk_acas, AcasSelection::SmartCardReader(_));
                    if ui.radio(reader_mode, "カードリーダーを指定する").clicked() && !reader_mode
                    {
                        self.fourk_acas = AcasSelection::SmartCardReader(String::new());
                    }
                    if let AcasSelection::SmartCardReader(reader) = &mut self.fourk_acas {
                        let candidates = self.fourk_reader_candidates.clone();
                        if !self.fourk_reader_candidates.is_empty() {
                            let selected_text = if reader.is_empty() {
                                "候補を選択".to_owned()
                            } else {
                                reader.clone()
                            };
                            egui::ComboBox::from_id_salt("fourk_card_reader")
                                .selected_text(selected_text)
                                .show_ui(ui, |ui| {
                                    for candidate in &candidates {
                                        ui.selectable_value(
                                            &mut *reader,
                                            candidate.clone(),
                                            candidate,
                                        );
                                    }
                                });
                        }
                        ui.text_edit_singleline(reader);
                        hint(
                            ui,
                            "候補を取得できない場合は、カードリーダー名を直接入力します。",
                        );
                    }
                    let proxy_mode = matches!(self.fourk_acas, AcasSelection::CasProxyServer(_));
                    if ui.radio(proxy_mode, "CasProxyServerを使う").clicked() && !proxy_mode {
                        self.fourk_acas =
                            AcasSelection::CasProxyServer("127.0.0.1:24000".to_owned());
                    }
                    if let AcasSelection::CasProxyServer(address) = &mut self.fourk_acas {
                        ui.text_edit_singleline(address);
                        hint(ui, "別PCまたは同じPCのCasProxyServerへ接続します。既定は127.0.0.1:24000です。");
                    }
                    hint(ui, "選んだDLLは通常チューナーとして登録せず、ラッパーだけをBS4K用に登録します。");
                }
            });
        }

        ui.add_space(10.0);
        ui.collapsing(
            egui::RichText::new("詳しい設定 (通常は変更不要)").size(17.0),
            |ui| {
                egui::Grid::new("advanced_grid_options")
                    .num_columns(2)
                    .spacing([16.0, 10.0])
                    .show(ui, |ui| {
                        ui.label("録画・視聴ソフトが接続するアドレス:");
                        ui.text_edit_singleline(&mut self.listen_addr);
                        ui.end_row();
                        ui.label("Webダッシュボードのアドレス:");
                        ui.text_edit_singleline(&mut self.web_listen_addr);
                        ui.end_row();
                    });
                hint(ui, "ポートを変更した場合、Windowsファイアウォールの許可ルールは実行ファイル単位で更新されます。");
            },
        );

        ui.add_space(20.0);
        ui.horizontal(|ui| {
            if secondary_button(ui, "◀ 戻る").clicked() {
                self.step = Step::SelectTuners;
            }
            if primary_button(ui, "次へ  ▶").clicked() {
                let needs_reader_probe = cfg!(windows)
                    && self.setup_4k
                    && matches!(self.fourk_acas, AcasSelection::SmartCardReader(ref name) if name.trim().is_empty())
                    && self.fourk_reader_candidates.is_empty();
                if needs_reader_probe {
                    self.start_fourk_probe();
                } else {
                    self.step = Step::Confirm;
                }
            }
        });
    }

    fn ui_confirm(&mut self, ui: &mut egui::Ui) {
        page_title(ui, &self.mode.step_label(4), "内容の確認");

        let selected_count =
            self.selected.iter().filter(|&&b| b).count() + self.manual_entries.len();
        let config_file_path = self.config_file_path();
        let db_file_path = self.db_file_path();

        card(ui, |ui| {
            egui::Grid::new("confirm_grid")
                .num_columns(2)
                .spacing([16.0, 10.0])
                .show(ui, |ui| {
                    ui.label("セットアップの種類:");
                    ui.label(self.mode.title());
                    ui.end_row();

                    ui.label("インストール先:");
                    ui.label(self.install_dir().display().to_string());
                    ui.end_row();

                    ui.label("設定ファイル:");
                    ui.label(config_file_path.display().to_string());
                    ui.end_row();

                    ui.label("データベース:");
                    ui.label(db_file_path.display().to_string());
                    ui.end_row();

                    ui.label("登録するチューナー数:");
                    ui.label(format!("{selected_count} 台"));
                    ui.end_row();

                    ui.label("アクセス範囲:");
                    ui.label(if self.lan_access {
                        "家のネットワーク(LAN)"
                    } else {
                        "このPCだけ"
                    });
                    ui.end_row();

                    ui.label("Mirakurun互換API:");
                    ui.label(if self.mirakurun_enabled {
                        "有効"
                    } else {
                        "無効"
                    });
                    ui.end_row();
                });

            if config_file_path.exists() || db_file_path.exists() {
                ui.add_space(10.0);
                if config_file_path.exists() {
                    ui.checkbox(
                        &mut self.overwrite_config,
                        "既存の設定ファイルを上書きする(チェックしない場合は既存のまま使用)",
                    );
                }
                if db_file_path.exists() {
                    ui.checkbox(
                        &mut self.recreate_db,
                        "既存のデータベースを作り直す(元のファイルは自動でバックアップされます)",
                    );
                }
            }
        });

        ui.add_space(12.0);

        card(ui, |ui| {
            ui.label(egui::RichText::new("追加で行うこと").size(19.0));
            ui.add_space(6.0);
            ui.label(if self.setup_preview {
                "ブラウザプレビュー: 有効"
            } else {
                "ブラウザプレビュー: 無効"
            });
            ui.label(if self.setup_tsreplace {
                "tsreplace: 準備する"
            } else {
                "tsreplace: 準備しない"
            });
            if recisdb_proxy::service::is_supported() {
                ui.add_space(10.0);
                ui.checkbox(
                    &mut self.register_service,
                    "OSのサービスとして登録し、PC起動時に自動で開始する",
                );
                if self.register_service {
                    ui.horizontal(|ui| {
                        ui.label("サービス名:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.service_name)
                                .desired_width(260.0)
                                .hint_text(recisdb_proxy::service::DEFAULT_SERVICE_NAME),
                        );
                    });
                    // 入力ミスをこの場で知らせる (登録は実行時に再検証される)。
                    if let Err(e) =
                        recisdb_proxy::service::sanitize_service_name(&self.service_name)
                    {
                        ui.colored_label(palette::DANGER, e.to_string());
                    }
                    if cfg!(windows) {
                        hint(
                            ui,
                            "登録には管理者権限が必要です。管理者として実行してください。",
                        );
                    } else {
                        ui.checkbox(
                            &mut self.service_user_scope,
                            "ログインユーザー単位で登録する(管理者権限なしで登録できますが、ログイン後にのみ動作します)",
                        );
                        if !self.service_user_scope {
                            hint(
                                ui,
                                "システム全体への登録には root 権限が必要です (sudo で実行してください)。",
                            );
                        }
                    }
                }
            }
        });

        if self.setup_rx.is_some() {
            ui.add_space(12.0);
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("セットアップを実行しています…");
                });
                if let Some(line) = self.log_lines.last() {
                    ui.label(line);
                }
            });
        }

        if let Some(err) = &self.setup_error {
            ui.add_space(12.0);
            error_box(ui, err);
        }

        ui.add_space(20.0);
        ui.horizontal(|ui| {
            let running = self.setup_rx.is_some();
            if ui
                .add_enabled(!running, egui::Button::new("◀ 戻る"))
                .clicked()
            {
                self.step = Step::Options;
            }
            if ui
                .add_enabled(
                    !running,
                    egui::Button::new("この内容でセットアップを実行  ▶"),
                )
                .clicked()
            {
                self.run_setup();
            }
        });
    }

    fn ui_done(&mut self, ui: &mut egui::Ui) {
        page_title(ui, "", "セットアップ完了！");

        card(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(200.0)
                .id_salt("done_log_scroll")
                .show(ui, |ui| {
                    for line in &self.log_lines {
                        ui.label(line);
                    }
                });
        });

        ui.add_space(16.0);

        if let Some(msg) = &self.launch_message {
            ui.label(msg.as_str());
            ui.add_space(10.0);
        }

        ui.horizontal(|ui| {
            if self.service_registered {
                // サービスが既に起動している。ここで実行ファイルを直接
                // 起動すると listen ポートが衝突するので、開くだけにする。
                if primary_button(ui, "ダッシュボードを開く  ▶").clicked() {
                    open_in_browser(&dashboard_url(&self.web_listen_addr));
                }
            } else if primary_button(ui, "recisdb-proxy を起動する  ▶").clicked() {
                self.launch_server_and_open_dashboard();
            }
            if secondary_button(ui, "終了").clicked() {
                std::process::exit(0);
            }
        });

        if self.service_registered {
            ui.add_space(6.0);
            hint(
                ui,
                "recisdb-proxy はサービスとして常時稼働します (PC起動時に自動で開始します)。",
            );
        }

        ui.add_space(16.0);
        card(ui, |ui| {
            ui.label(egui::RichText::new("このあと必要な作業").size(19.0));
            ui.add_space(6.0);
            ui.label(format!(
                "・「{}{}{}」の中身を、TVTest/EDCB を動かすPCの BonDriver フォルダにコピーしてください。",
                self.install_dir().display(),
                std::path::MAIN_SEPARATOR,
                setup_helpers::CLIENT_CONFIG_DIR
            ));
            hint(
                ui,
                "(接続先アドレス入りの BonDriver_NetworkProxy.ini と手順の README が入っています)",
            );
            ui.add_space(6.0);
            ui.label(format!(
                "・Webダッシュボード ({}) からチューナーの詳細設定や、チャンネルスキャン後の\
                 TVTest用 .ch2 / EDCB用 ChSet4/ChSet5 のダウンロードができます(「クライアント設定」タブ)。",
                dashboard_url(&self.web_listen_addr)
            ));
        });

        if !self.bulk_update_dir.trim().is_empty() {
            ui.add_space(16.0);
            card(ui, |ui| {
                ui.label(egui::RichText::new("既存クライアントDLLの一括更新").size(19.0));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("更新先フォルダ:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.bulk_update_dir).desired_width(420.0),
                    );
                });
                hint(
                    ui,
                    "指定したフォルダ(サブフォルダ含む)にある \"BonDriver_NetworkProxy\" で始まるDLLを、\
                     今回配置した最新版の内容でまとめて上書きします。",
                );
                ui.add_space(8.0);
                if ui.button("今すぐ一括更新を実行する").clicked() {
                    self.run_bulk_dll_update();
                }

                if let Some(err) = &self.bulk_update_error {
                    ui.add_space(8.0);
                    error_box(ui, err);
                }
                if self.bulk_update_ran && self.bulk_update_error.is_none() {
                    ui.add_space(8.0);
                    if self.bulk_update_log.is_empty() {
                        ui.label("対象のDLLは見つかりませんでした。");
                    } else {
                        egui::ScrollArea::vertical()
                            .max_height(160.0)
                            .id_salt("bulk_update_log_scroll")
                            .show(ui, |ui| {
                                for line in &self.bulk_update_log {
                                    ui.label(line);
                                }
                            });
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_url_extracts_port() {
        assert_eq!(dashboard_url("0.0.0.0:40080"), "http://localhost:40080");
        assert_eq!(dashboard_url("127.0.0.1:8080"), "http://localhost:8080");
    }

    #[test]
    fn default_install_location_matches_spec() {
        let app = SetupApp::new();
        if cfg!(windows) {
            assert_eq!(app.install_location, r"C:\DTV\recisdb-proxy-rs");
        }
    }

    #[test]
    fn setup_exe_dir_returns_a_dir_containing_the_test_binary() {
        let dir = setup_exe_dir().expect("current test binary must have a parent dir");
        assert!(dir.is_dir());
    }

    #[test]
    fn run_bulk_dll_update_requires_target_dir() {
        let mut app = SetupApp::new();
        assert!(app.bulk_update_dir.trim().is_empty());

        app.run_bulk_dll_update();

        assert!(app.bulk_update_ran);
        assert!(app.bulk_update_error.is_some());
        assert!(app.bulk_update_log.is_empty());
    }

    #[test]
    fn run_bulk_dll_update_reports_error_when_source_dll_missing() {
        // インストールを実行していない(セットアップ未完了)状況では
        // クライアント配布用DLLがまだ存在しないため、エラーとして扱われる。
        let base =
            std::env::temp_dir().join(format!("run_bulk_dll_update_test_{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let target_dir = base.join("target");
        std::fs::create_dir_all(&target_dir).unwrap();

        let mut app = SetupApp::new();
        app.install_location = base.join("install").to_string_lossy().to_string();
        app.bulk_update_dir = target_dir.to_string_lossy().to_string();

        app.run_bulk_dll_update();

        assert!(app.bulk_update_ran);
        assert!(app.bulk_update_error.is_some());

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn install_dir_is_always_absolute() {
        // install_location がファイル名だけ(相対パス、親ディレクトリなし)の
        // 場合でも install_dir() は絶対パスを返さなければならない。相対の
        // ままだと、ドライバインストールの昇格プロセス(既定の作業
        // ディレクトリが C:\Windows\System32 になる)から見て解決先が
        // ずれてしまう。
        let mut app = SetupApp::new();
        app.install_location = "recisdb-proxy-rs".to_string();
        assert!(app.install_dir().is_absolute());

        app.install_location = "sub/dir/recisdb-proxy-rs".to_string();
        assert!(app.install_dir().is_absolute());
    }

    #[test]
    fn config_and_db_paths_are_derived_from_install_location() {
        let mut app = SetupApp::new();
        app.install_location = "C:\\DTV\\recisdb-proxy-rs".to_string();
        assert_eq!(
            app.config_file_path(),
            PathBuf::from("C:\\DTV\\recisdb-proxy-rs\\recisdb-proxy.toml")
        );
        assert_eq!(
            app.db_file_path(),
            PathBuf::from("C:\\DTV\\recisdb-proxy-rs\\recisdb-proxy.db")
        );
    }

    // ---- モード選択 -------------------------------------------------------

    #[test]
    fn starts_at_mode_select() {
        let app = SetupApp::new();
        assert_eq!(app.step, Step::ModeSelect);
    }

    #[test]
    fn dll_only_mode_jumps_straight_to_the_dll_screen() {
        // DLL差し替えだけをしたい人に、インストール先やチューナー検出の
        // 画面を通らせない。
        let mut app = SetupApp::new();
        app.choose_mode(SetupMode::DllOnly);
        assert_eq!(app.step, Step::DllOnly);
        assert_eq!(app.mode, SetupMode::DllOnly);
    }

    #[test]
    fn install_modes_start_from_location() {
        for mode in [SetupMode::FullAuto, SetupMode::Manual] {
            let mut app = SetupApp::new();
            app.choose_mode(mode);
            assert_eq!(app.step, Step::Location);
            assert_eq!(app.mode, mode);
        }
    }

    #[test]
    fn manual_mode_does_not_preinstall_optional_extras() {
        // 手動セットアップではダウンロードを伴うプレビュー準備を既定でOFFに
        // する (全自動との違いがここに出る)。
        let mut app = SetupApp::new();
        app.choose_mode(SetupMode::Manual);
        assert!(!app.setup_preview);

        let mut app = SetupApp::new();
        app.choose_mode(SetupMode::FullAuto);
        assert!(app.setup_preview);
    }

    #[test]
    fn only_full_auto_drives_driver_installation() {
        let mut app = SetupApp::new();
        app.choose_mode(SetupMode::Manual);
        assert!(!app.is_full_auto());

        app.choose_mode(SetupMode::FullAuto);
        assert!(app.is_full_auto());
    }

    #[test]
    fn explicit_source_dll_wins_over_installed_bundle() {
        let mut app = SetupApp::new();
        app.install_location = "C:\\DTV\\recisdb-proxy-rs".to_string();
        app.dll_source_path = "D:\\dl\\BonDriver_NetworkProxy.dll".to_string();
        assert_eq!(
            app.resolve_source_dll(),
            PathBuf::from("D:\\dl\\BonDriver_NetworkProxy.dll")
        );
    }

    #[test]
    fn source_dll_falls_back_to_the_client_bundle() {
        // 明示指定が無く、インストール先にも配布用DLLが無い場合は
        // このツール自身の隣を探す (リリースzipを展開しただけの状態)。
        let base = std::env::temp_dir().join(format!("resolve_source_dll_{}", std::process::id()));
        std::fs::create_dir_all(base.join(setup_helpers::CLIENT_CONFIG_DIR)).unwrap();
        let bundled = base
            .join(setup_helpers::CLIENT_CONFIG_DIR)
            .join("BonDriver_NetworkProxy.dll");
        std::fs::write(&bundled, b"dummy").unwrap();

        let mut app = SetupApp::new();
        app.install_location = base.to_string_lossy().to_string();
        assert_eq!(app.resolve_source_dll(), bundled);

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn failed_driver_installs_are_not_retried_forever() {
        // 全自動モードで、失敗したチューナーを何度も再試行して先に進めなく
        // なるのを防ぐ。
        let mut app = SetupApp::new();
        app.detected = vec![];
        assert_eq!(app.next_driver_install_target(), None);
    }
}

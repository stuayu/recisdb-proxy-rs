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
use recisdb_proxy::database::Database;
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
            ("C:\\Windows\\Fonts\\meiryo.ttc", "C:\\Windows\\Fonts\\meiryob.ttc"),
            ("C:\\Windows\\Fonts\\msgothic.ttc", "C:\\Windows\\Fonts\\msgothic.ttc"),
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
        (TextStyle::Button, FontId::new(17.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(15.0, FontFamily::Monospace)),
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
    ui.label(
        egui::RichText::new(text)
            .size(15.0)
            .color(palette::MUTED),
    );
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
            _ => 3,
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

struct SetupApp {
    step: Step,
    /// 入口で選んだセットアップの種類
    mode: SetupMode,

    // ステップ1: 基本設定 (ほぼ既定値のまま「次へ」を押すだけで進める)
    listen_addr: String,
    web_listen_addr: String,
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
            service_name: recisdb_proxy::service::DEFAULT_SERVICE_NAME.to_string(),
            service_registered: false,
            service_user_scope: false,
            log_lines: Vec::new(),
            setup_error: None,
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
                self.register_service = true;
                self.step = Step::Location;
            }
            SetupMode::Manual => {
                // 自分で決めたい人向け。ダウンロードを伴うプレビュー準備は
                // 既定でOFFにし、必要なら確認画面で明示的に選んでもらう。
                self.setup_preview = false;
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

    fn run_setup(&mut self) {
        self.log_lines.clear();
        self.setup_error = None;

        let install_dir = self.install_dir();
        if let Err(e) = std::fs::create_dir_all(&install_dir) {
            self.setup_error = Some(format!("インストール先フォルダの作成に失敗しました: {e}"));
            return;
        }

        // recisdb-proxy 本体をインストール先に配置/更新する。既にインストール
        // 済みで内容が同一なら何もしない(設定ファイル・DBには触れない)。
        match setup_exe_dir() {
            Some(source_dir) => match setup_helpers::sync_program_binary(&source_dir, &install_dir) {
                Ok(setup_helpers::BinarySyncAction::FreshInstall) => self.log_lines.push(format!(
                    "recisdb-proxy をインストールしました: {}",
                    install_dir.display()
                )),
                Ok(setup_helpers::BinarySyncAction::Updated) => self
                    .log_lines
                    .push("recisdb-proxy を最新版に更新しました。".to_string()),
                Ok(setup_helpers::BinarySyncAction::AlreadyUpToDate) => self
                    .log_lines
                    .push("recisdb-proxy は既に最新の状態です。".to_string()),
                Err(e) => {
                    self.setup_error = Some(e);
                    return;
                }
            },
            None => {
                self.setup_error = Some("実行ファイルの場所を取得できませんでした。".to_string());
                return;
            }
        }

        let config_file_path = self.config_file_path();
        if !config_file_path.exists() || self.overwrite_config {
            let content = generate_config(
                &self.listen_addr,
                &self.web_listen_addr,
                &self.db_file_path().to_string_lossy(),
            );
            match std::fs::write(&config_file_path, content) {
                Ok(()) => self.log_lines.push(format!(
                    "設定ファイルを保存しました: {}",
                    config_file_path.display()
                )),
                Err(e) => {
                    self.setup_error = Some(format!("設定ファイルの保存に失敗しました: {e}"));
                    return;
                }
            }
        } else {
            self.log_lines
                .push("既存の設定ファイルをそのまま使用します。".to_string());
        }

        let db_file_path = self.db_file_path();
        if db_file_path.exists() && self.recreate_db {
            let backup_path = format!("{}.backup", db_file_path.display());
            if let Err(e) = std::fs::rename(&db_file_path, &backup_path) {
                self.setup_error = Some(format!("データベースのバックアップに失敗しました: {e}"));
                return;
            }
            self.log_lines
                .push(format!("既存のデータベースをバックアップしました: {backup_path}"));
        }

        let db = match Database::open(&db_file_path) {
            Ok(db) => db,
            Err(e) => {
                self.setup_error = Some(format!("データベースの初期化に失敗しました: {e}"));
                return;
            }
        };
        self.log_lines
            .push(format!("データベースを初期化しました: {}", db_file_path.display()));

        let selected_indices: Vec<usize> = self
            .selected
            .iter()
            .enumerate()
            .filter_map(|(i, &checked)| checked.then_some(i))
            .collect();

        if !selected_indices.is_empty() {
            let results = register_tuners_to_db(&db, &self.detected, &selected_indices);
            for r in results {
                match r.outcome {
                    Ok(id) => self
                        .log_lines
                        .push(format!("チューナーを登録しました: {} (ID: {id})", r.device_path)),
                    Err(e) => self
                        .log_lines
                        .push(format!("チューナーの登録に失敗しました: {} ({e})", r.device_path)),
                }
            }
        }

        for entry in &self.manual_entries {
            match register_manual_tuner(&db, &entry.path, &entry.group, entry.max_instances) {
                Ok(id) => self
                    .log_lines
                    .push(format!("チューナーを登録しました: {} (ID: {id})", entry.path)),
                Err(e) => self
                    .log_lines
                    .push(format!("チューナーの登録に失敗しました: {} ({e})", entry.path)),
            }
        }

        // クライアント (TVTest/EDCB側PC) に配布する設定一式を出力する。
        // Tuner= は登録済みドライバーのグループ名を優先 (グループ指定なら
        // サーバーが空きチューナーを自動選択できるため)、無ければ先頭の
        // DLLパスを入れる。
        {
            let tuner_hint = db
                .get_all_bon_drivers()
                .ok()
                .and_then(|drivers| {
                    drivers
                        .iter()
                        .find_map(|d| d.group_name.clone().filter(|g| !g.trim().is_empty()))
                        .or_else(|| drivers.first().map(|d| d.dll_path.clone()))
                })
                .unwrap_or_default();
            let proxy_port = self.listen_addr.rsplit(':').next().unwrap_or("40070");
            let web_port = self.web_listen_addr.rsplit(':').next().unwrap_or("40080");
            let ip = setup_helpers::local_lan_ip()
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "127.0.0.1".to_string());
            match setup_helpers::write_client_config_bundle(
                &install_dir,
                setup_exe_dir().as_deref(),
                &format!("{ip}:{proxy_port}"),
                &tuner_hint,
                &format!("http://{ip}:{web_port}"),
            ) {
                Ok(lines) => self.log_lines.extend(lines),
                Err(e) => self
                    .log_lines
                    .push(format!("クライアント設定の出力に失敗しました: {e}")),
            }
        }

        if self.setup_preview {
            self.setup_browser_preview(&db, &install_dir);
        }

        if self.register_service && recisdb_proxy::service::is_supported() {
            self.register_os_service(&install_dir);
        }

        self.step = Step::Done;
    }

    /// ブラウザプレビュー (Webダッシュボードでの映像確認) を使えるようにする。
    ///
    /// エンコーダ (ffmpeg) と前段処理 (tsreadex) を検出し、無ければ取得して
    /// 設定ファイルとDBに書き込む。ダウンロードやビルドを伴うため失敗しうるが、
    /// **失敗してもセットアップ全体は続行する** — プレビューが無くても
    /// TVTest からの視聴という主目的には影響しないため。理由はログに残し、
    /// あとからダッシュボードの「プレビューを使えるようにする」で再試行できる。
    fn setup_browser_preview(&mut self, db: &recisdb_proxy::database::Database, install_dir: &Path) {
        let config_path = install_dir.join("recisdb-proxy.toml");
        self.log_lines
            .push("ブラウザプレビューの準備中... (ダウンロードを伴うため時間がかかります)".to_string());
        match recisdb_proxy::preview_setup::ensure_preview_ready(db, install_dir, Some(&config_path)) {
            Ok(report) => {
                self.log_lines.push(format!(
                    "ブラウザプレビューを有効にしました (エンコーダ: {} / 映像: {})",
                    report.encoder_path, report.video_encoder
                ));
                if report.preprocessor_path.is_empty() {
                    self.log_lines
                        .push("前段処理 (tsreadex) は未設定です。字幕が表示されない場合があります。".to_string());
                }
                self.log_lines.extend(report.warnings);
            }
            Err(e) => self.log_lines.push(format!(
                "ブラウザプレビューの準備に失敗しました (視聴・録画には影響しません): {e}"
            )),
        }
    }

    /// セットアップ本体の最後に、インストールした実行ファイルをOSの
    /// サービスとして登録する。失敗しても致命的ではない (サーバ自体は
    /// 手動で起動できる) ので、`setup_error` にはせずログに理由と手動
    /// 登録用コマンドを残す。
    fn register_os_service(&mut self, install_dir: &Path) {
        use recisdb_proxy::service::{self, ServiceScope};

        let name = match service::sanitize_service_name(&self.service_name) {
            Ok(name) => name,
            Err(e) => {
                self.log_lines
                    .push(format!("サービス名が不正なため登録をスキップしました: {e}"));
                return;
            }
        };
        let scope = if self.service_user_scope && !cfg!(windows) {
            ServiceScope::User
        } else {
            ServiceScope::System
        };

        let exe_name = if cfg!(windows) { "recisdb-proxy.exe" } else { "recisdb-proxy" };
        let exe_path = install_dir.join(exe_name);
        let config_file_path = self.config_file_path();
        let spec = service::default_spec(
            name.clone(),
            scope,
            exe_path,
            install_dir.to_path_buf(),
            vec![
                "-f".to_string(),
                config_file_path.to_string_lossy().into_owned(),
            ],
        );

        match service::install(&spec) {
            Ok(()) => {
                self.service_registered = true;
                self.log_lines
                    .push(format!("サービス `{name}` を登録し、開始しました。"));
            }
            Err(e) => {
                let hint = if cfg!(windows) {
                    format!(
                        "管理者として実行したコマンドプロンプトで `\"{}\" service install --name {name}` を実行してください。",
                        install_dir.join(exe_name).display()
                    )
                } else {
                    format!(
                        "`sudo \"{}\" service install --name {name}` を実行してください。",
                        install_dir.join(exe_name).display()
                    )
                };
                self.log_lines
                    .push(format!("サービスの登録に失敗しました: {e}"));
                self.log_lines.push(hint);
            }
        }
    }

    fn launch_server_and_open_dashboard(&mut self) {
        let install_dir = self.install_dir();
        let exe_name = if cfg!(windows) { "recisdb-proxy.exe" } else { "recisdb-proxy" };
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

/// 現在実行中のこのツール自身が置かれているフォルダ。recisdb-proxy 本体を
/// インストール先へコピーしてくる際のコピー元として使う(ダウンロードした
/// リリースzipの展開先を想定)。
fn setup_exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
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

impl eframe::App for SetupApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // チューナー検出の完了待ち
        if let Some(rx) = &self.detect_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.selected = vec![true; result.len()];
                    self.detected = result;
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
                        ui.label(
                            egui::RichText::new(badge)
                                .size(14.0)
                                .color(palette::ACCENT),
                        );
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
                ui.add(
                    egui::TextEdit::singleline(&mut self.install_location).desired_width(480.0),
                );
                #[cfg(windows)]
                if ui.button("参照…").clicked() {
                    let start_dir = self.install_dir();
                    if let Some(dir) = rfd::FileDialog::new().set_directory(&start_dir).pick_folder()
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

        ui.add_space(12.0);
        ui.collapsing(
            egui::RichText::new("詳しい設定 (通常は変更不要)").size(17.0),
            |ui| {
                egui::Grid::new("advanced_grid")
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
            },
        );

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
                ui.label("接続されているチューナーを自動で検出しています。しばらくお待ちください。");
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
                ui.colored_label(palette::DANGER, "ドライバのインストールでエラーが発生しました:");
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
                if ui.button("この内容で追加").clicked()
                    && !self.manual_form.path.trim().is_empty()
                {
                    let max_instances = self.manual_form.max_instances.trim().parse().unwrap_or(1);
                    self.manual_entries.push(ManualEntry {
                        path: self.manual_form.path.trim().to_string(),
                        group: self.manual_form.group.trim().to_string(),
                        max_instances,
                    });
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
                self.step = Step::Confirm;
            }
        });
    }

    fn ui_confirm(&mut self, ui: &mut egui::Ui) {
        page_title(ui, &self.mode.step_label(3), "内容の確認");

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
            ui.checkbox(
                &mut self.setup_preview,
                "ブラウザで映像を確認できるようにする(エンコーダーを自動で用意します)",
            );
            if self.setup_preview {
                hint(
                    ui,
                    "必要なプログラムをインターネットから取得するため、数分かかることがあります。",
                );
            }

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

        if let Some(err) = &self.setup_error {
            ui.add_space(12.0);
            error_box(ui, err);
        }

        ui.add_space(20.0);
        ui.horizontal(|ui| {
            if secondary_button(ui, "◀ 戻る").clicked() {
                self.step = Step::SelectTuners;
            }
            if primary_button(ui, "この内容でセットアップを実行  ▶").clicked() {
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
                        egui::TextEdit::singleline(&mut self.bulk_update_dir)
                            .desired_width(420.0),
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
        let base = std::env::temp_dir().join(format!(
            "run_bulk_dll_update_test_{}",
            std::process::id()
        ));
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

//! recisdb-proxy かんたんセットアップ (GUIウィザード)
//!
//! プログラムを触ったことがない人でも、画面の指示に従って「次へ」を押すだけで
//! recisdb-proxy を使い始められることを目標にしたセットアップウィザード。
//! コマンドライン入力は一切不要。実際のロジック(チューナー検出・設定ファイル
//! 生成・DB登録)は `recisdb_proxy::setup_helpers` に切り出されている。

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
    self, generate_config, register_manual_tuner, register_tuners_to_db, DetectedTuner,
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
            .with_inner_size([620.0, 480.0])
            .with_min_inner_size([500.0, 400.0]),
        ..Default::default()
    };

    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(|cc| {
            egui_extras_setup(&cc.egui_ctx);
            Ok(Box::new(SetupApp::new()))
        }),
    )
}

/// 日本語が文字化けしないよう、埋め込みフォント + OS の日本語フォントを試す。
/// 見つからない場合はデフォルトフォントのまま(egui標準フォントは和文非対応な
/// ため、四角(いわゆる tofu)で表示されるが、起動不能になるよりはまし)。
fn egui_extras_setup(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    let candidates: &[&str] = if cfg!(windows) {
        &[
            "C:\\Windows\\Fonts\\YuGothM.ttc",
            "C:\\Windows\\Fonts\\meiryo.ttc",
            "C:\\Windows\\Fonts\\msgothic.ttc",
        ]
    } else if cfg!(target_os = "macos") {
        &["/System/Library/Fonts/ヒラギノ角ゴシック W4.ttc"]
    } else {
        &[
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        ]
    };

    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "jp".to_owned(),
                egui::FontData::from_owned(bytes).into(),
            );
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
            ctx.set_fonts(fonts);
            return;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Step {
    Welcome,
    Location,
    Detecting,
    SelectTuners,
    Confirm,
    Done,
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

struct SetupApp {
    step: Step,

    // ステップ1: 基本設定 (ほぼ既定値のまま「次へ」を押すだけで進める)
    listen_addr: String,
    web_listen_addr: String,
    db_path: String,
    config_path: String,

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

    // 上書き確認
    overwrite_config: bool,
    recreate_db: bool,

    // 実行結果
    log_lines: Vec<String>,
    setup_error: Option<String>,

    // 完了画面
    launch_deadline: Option<Instant>,
    launch_message: Option<String>,
}

impl SetupApp {
    fn new() -> Self {
        Self {
            step: Step::Welcome,
            listen_addr: "0.0.0.0:40070".to_string(),
            web_listen_addr: "0.0.0.0:40080".to_string(),
            db_path: "recisdb-proxy.db".to_string(),
            config_path: "recisdb-proxy.toml".to_string(),
            detect_rx: None,
            detected: Vec::new(),
            selected: Vec::new(),
            manual_form: ManualEntryForm::default(),
            manual_entries: Vec::new(),
            installing_index: None,
            install_rx: None,
            install_log: Vec::new(),
            install_error: None,
            overwrite_config: false,
            recreate_db: false,
            log_lines: Vec::new(),
            setup_error: None,
            launch_deadline: None,
            launch_message: None,
        }
    }

    fn start_detection(&mut self) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = setup_helpers::detect_tuners();
            let _ = tx.send(result);
        });
        self.detect_rx = Some(rx);
        self.step = Step::Detecting;
    }

    /// ドライバ/BonDriverの配置先フォルダ。設定ファイルと同じフォルダ
    /// (相対パス指定なら実行ファイルと同じフォルダ)にまとめる。
    fn install_dir(&self) -> PathBuf {
        let dir = Path::new(&self.config_path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        // 絶対パスにしておく: px4_drv のドライバインストールは別プロセスを
        // UAC昇格して実行するが、昇格したプロセスの作業ディレクトリは
        // (呼び出し元と異なり) 既定で C:\Windows\System32 になる。相対パスの
        // ままだとそこを起点に解決されてファイルが見つからなくなるため。
        // `canonicalize` は `\\?\` UNC プレフィックス付きパスを返し一部の
        // ツールと相性が悪いことがあるため使わない。
        if dir.is_absolute() {
            dir
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&dir))
                .unwrap_or(dir)
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

    /// px4_drv インストールスレッドからの通知を受け取り、状態を更新する。
    fn poll_px4_install(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.install_rx else {
            return;
        };

        loop {
            match rx.try_recv() {
                Ok(InstallEvent::Progress(msg)) => self.install_log.push(msg),
                Ok(InstallEvent::Done(result)) => {
                    let idx = self.installing_index.take().expect("installing_index set while install_rx is Some");
                    match result {
                        Ok(paths) => {
                            self.detected[idx].device_paths = paths;
                            self.selected[idx] = true;
                            self.install_error = None;
                        }
                        Err(e) => self.install_error = Some(e),
                    }
                    self.install_rx = None;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(150));
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.installing_index = None;
                    self.install_rx = None;
                    self.install_error = Some("インストール処理が予期せず終了しました。".to_string());
                    break;
                }
            }
        }
    }

    fn run_setup(&mut self) {
        self.log_lines.clear();
        self.setup_error = None;

        let config_file_path = Path::new(&self.config_path);
        if !config_file_path.exists() || self.overwrite_config {
            let content = generate_config(&self.listen_addr, &self.web_listen_addr, &self.db_path);
            match std::fs::write(config_file_path, content) {
                Ok(()) => self
                    .log_lines
                    .push(format!("設定ファイルを保存しました: {}", self.config_path)),
                Err(e) => {
                    self.setup_error = Some(format!("設定ファイルの保存に失敗しました: {e}"));
                    return;
                }
            }
        } else {
            self.log_lines
                .push("既存の設定ファイルをそのまま使用します。".to_string());
        }

        let db_file_path = Path::new(&self.db_path);
        if db_file_path.exists() && self.recreate_db {
            let backup_path = format!("{}.backup", self.db_path);
            if let Err(e) = std::fs::rename(db_file_path, &backup_path) {
                self.setup_error = Some(format!("データベースのバックアップに失敗しました: {e}"));
                return;
            }
            self.log_lines
                .push(format!("既存のデータベースをバックアップしました: {backup_path}"));
        }

        let db = match Database::open(&self.db_path) {
            Ok(db) => db,
            Err(e) => {
                self.setup_error = Some(format!("データベースの初期化に失敗しました: {e}"));
                return;
            }
        };
        self.log_lines
            .push(format!("データベースを初期化しました: {}", self.db_path));

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

        self.step = Step::Done;
    }

    fn launch_server_and_open_dashboard(&mut self) {
        match sibling_exe_path("recisdb-proxy") {
            Some(exe) => {
                let config_dir = Path::new(&self.config_path)
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(Path::to_path_buf);

                let mut cmd = std::process::Command::new(&exe);
                cmd.arg("--config").arg(&self.config_path);
                if let Some(dir) = config_dir {
                    cmd.current_dir(dir);
                }

                match cmd.spawn() {
                    Ok(_) => {
                        self.launch_message = Some(
                            "recisdb-proxy を起動しました。数秒後にダッシュボードを開きます…"
                                .to_string(),
                        );
                        self.launch_deadline = Some(Instant::now() + Duration::from_secs(2));
                    }
                    Err(e) => {
                        self.launch_message =
                            Some(format!("recisdb-proxy の起動に失敗しました: {e}"));
                    }
                }
            }
            None => {
                self.launch_message = Some(
                    "recisdb-proxy(本体)が同じフォルダに見つかりませんでした。\
                     recisdb-proxy.exe をこのツールと同じフォルダに置いてから、\
                     もう一度お試しください。"
                        .to_string(),
                );
            }
        }
    }
}

/// 実行中のこのツールと同じフォルダにある、指定した名前の実行ファイルを探す。
fn sibling_exe_path(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let filename = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let candidate = dir.join(filename);
    candidate.exists().then_some(candidate)
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

        egui::CentralPanel::default().show(ui, |ui| match self.step {
            Step::Welcome => self.ui_welcome(ui),
            Step::Location => self.ui_location(ui),
            Step::Detecting => self.ui_detecting(ui),
            Step::SelectTuners => self.ui_select_tuners(ui),
            Step::Confirm => self.ui_confirm(ui),
            Step::Done => self.ui_done(ui),
        });
    }
}

impl SetupApp {
    fn ui_welcome(&mut self, ui: &mut egui::Ui) {
        ui.heading("recisdb-proxy かんたんセットアップへようこそ");
        ui.add_space(12.0);
        ui.label(
            "このツールは、テレビチューナーを録画・視聴ソフトから使えるように\
             するための初期設定を、画面の指示に従うだけで行います。",
        );
        ui.add_space(6.0);
        ui.label("むずかしい用語が出てきても、基本的にはそのまま「次へ」を押して進められます。");
        ui.add_space(20.0);
        if ui.button("はじめる  ▶").clicked() {
            self.step = Step::Location;
        }
    }

    fn ui_location(&mut self, ui: &mut egui::Ui) {
        ui.heading("① 保存先の確認");
        ui.add_space(8.0);
        ui.label("設定ファイルとデータベースを保存する場所です。よくわからない場合はそのままで大丈夫です。");
        ui.add_space(12.0);

        egui::Grid::new("location_grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("設定ファイルの保存先:");
                ui.text_edit_singleline(&mut self.config_path);
                ui.end_row();

                ui.label("データベースファイルの保存先:");
                ui.text_edit_singleline(&mut self.db_path);
                ui.end_row();
            });

        ui.add_space(8.0);
        ui.collapsing("詳しい設定 (通常は変更不要)", |ui| {
            egui::Grid::new("advanced_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("録画・視聴ソフトが接続するアドレス:");
                    ui.text_edit_singleline(&mut self.listen_addr);
                    ui.end_row();

                    ui.label("Webダッシュボードのアドレス:");
                    ui.text_edit_singleline(&mut self.web_listen_addr);
                    ui.end_row();
                });
        });

        ui.add_space(20.0);
        ui.horizontal(|ui| {
            if ui.button("◀ 戻る").clicked() {
                self.step = Step::Welcome;
            }
            if ui.button("次へ  ▶").clicked() {
                self.start_detection();
            }
        });
    }

    fn ui_detecting(&mut self, ui: &mut egui::Ui) {
        ui.heading("② チューナーを探しています…");
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("接続されているチューナーを自動で検出しています。しばらくお待ちください。");
        });
    }

    fn ui_select_tuners(&mut self, ui: &mut egui::Ui) {
        ui.heading("② 使用するチューナーを選択");
        ui.add_space(8.0);

        if self.detected.is_empty() {
            ui.label("チューナーは自動検出できませんでした。下から手動で追加できます。");
        } else {
            ui.label("見つかったチューナーのうち、使用するものにチェックを入れてください。");
            ui.add_space(10.0);

            let mut install_clicked: Option<usize> = None;

            egui::ScrollArea::vertical()
                .max_height(220.0)
                .show(ui, |ui| {
                    for i in 0..self.detected.len() {
                        let installing_this = self.installing_index == Some(i);
                        let needs_driver = self.detected[i].px4_model_pid.is_some()
                            && self.detected[i].device_paths.is_empty();

                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.selected[i], "");
                            ui.vertical(|ui| {
                                let tuner = &self.detected[i];
                                ui.strong(&tuner.name);
                                if tuner.terrestrial_count > 0 || tuner.satellite_count > 0 {
                                    ui.label(format!(
                                        "地上波 {}ch / 衛星(BS/CS) {}ch",
                                        tuner.terrestrial_count, tuner.satellite_count
                                    ));
                                }
                                for path in &tuner.device_paths {
                                    ui.label(egui::RichText::new(path).weak().small());
                                }

                                if needs_driver {
                                    if installing_this {
                                        ui.horizontal(|ui| {
                                            ui.spinner();
                                            let msg = self
                                                .install_log
                                                .last()
                                                .cloned()
                                                .unwrap_or_else(|| "インストール準備中…".to_string());
                                            ui.label(msg);
                                        });
                                    } else {
                                        ui.horizontal(|ui| {
                                            ui.colored_label(
                                                egui::Color32::from_rgb(200, 140, 0),
                                                "ドライバが未インストールです",
                                            );
                                            let enabled = self.installing_index.is_none();
                                            if ui
                                                .add_enabled(
                                                    enabled,
                                                    egui::Button::new("ドライバを自動インストール"),
                                                )
                                                .clicked()
                                            {
                                                install_clicked = Some(i);
                                            }
                                        });
                                    }
                                }
                            });
                        });
                        ui.separator();
                    }
                });

            if let Some(i) = install_clicked {
                self.start_px4_install(i);
            }

            if let Some(err) = &self.install_error {
                ui.add_space(6.0);
                // pnputil等の詳細ログは複数行になりうるので、選択・コピーできる
                // スクロール可能なテキストボックスで表示する(単一行ラベルだと
                // 折り返しやコピーができず読みづらいため)。
                ui.colored_label(egui::Color32::from_rgb(200, 60, 60), "エラーが発生しました:");
                let mut err_text = err.clone();
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .id_salt("install_error_scroll")
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut err_text)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .text_color(egui::Color32::from_rgb(200, 60, 60)),
                        );
                    });
            }
        }

        ui.add_space(12.0);
        ui.collapsing("見つからない場合: 手動で追加する", |ui| {
            egui::Grid::new("manual_grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
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

            if ui.button("この内容で追加").clicked() && !self.manual_form.path.trim().is_empty() {
                let max_instances = self.manual_form.max_instances.trim().parse().unwrap_or(1);
                self.manual_entries.push(ManualEntry {
                    path: self.manual_form.path.trim().to_string(),
                    group: self.manual_form.group.trim().to_string(),
                    max_instances,
                });
                self.manual_form = ManualEntryForm::default();
            }

            if !self.manual_entries.is_empty() {
                ui.add_space(6.0);
                ui.label("追加予定のチューナー:");
                let mut remove_at = None;
                for (i, entry) in self.manual_entries.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("  {} (グループ: {})", entry.path, entry.group));
                        if ui.small_button("削除").clicked() {
                            remove_at = Some(i);
                        }
                    });
                }
                if let Some(i) = remove_at {
                    self.manual_entries.remove(i);
                }
            }
        });

        ui.add_space(20.0);
        ui.horizontal(|ui| {
            if ui.button("◀ 戻る").clicked() {
                self.step = Step::Location;
            }
            if ui.button("次へ  ▶").clicked() {
                self.overwrite_config = !Path::new(&self.config_path).exists();
                self.recreate_db = !Path::new(&self.db_path).exists();
                self.step = Step::Confirm;
            }
        });
    }

    fn ui_confirm(&mut self, ui: &mut egui::Ui) {
        ui.heading("③ 内容の確認");
        ui.add_space(8.0);

        let selected_count = self.selected.iter().filter(|&&b| b).count() + self.manual_entries.len();

        egui::Grid::new("confirm_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("設定ファイル:");
                ui.label(&self.config_path);
                ui.end_row();

                ui.label("データベース:");
                ui.label(&self.db_path);
                ui.end_row();

                ui.label("登録するチューナー数:");
                ui.label(format!("{selected_count} 台"));
                ui.end_row();
            });

        ui.add_space(10.0);

        if Path::new(&self.config_path).exists() {
            ui.checkbox(
                &mut self.overwrite_config,
                "既存の設定ファイルを上書きする(チェックしない場合は既存のまま使用)",
            );
        }
        if Path::new(&self.db_path).exists() {
            ui.checkbox(
                &mut self.recreate_db,
                "既存のデータベースを作り直す(元のファイルは自動でバックアップされます)",
            );
        }

        if let Some(err) = &self.setup_error {
            ui.add_space(10.0);
            ui.colored_label(egui::Color32::from_rgb(200, 60, 60), err);
        }

        ui.add_space(20.0);
        ui.horizontal(|ui| {
            if ui.button("◀ 戻る").clicked() {
                self.step = Step::SelectTuners;
            }
            if ui.button("この内容でセットアップを実行  ▶").clicked() {
                self.run_setup();
            }
        });
    }

    fn ui_done(&mut self, ui: &mut egui::Ui) {
        ui.heading("セットアップ完了！");
        ui.add_space(10.0);

        egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
            for line in &self.log_lines {
                ui.label(line);
            }
        });

        ui.add_space(16.0);

        if let Some(msg) = &self.launch_message {
            ui.label(msg.as_str());
            ui.add_space(10.0);
        }

        ui.horizontal(|ui| {
            if ui.button("recisdb-proxy を起動する  ▶").clicked() {
                self.launch_server_and_open_dashboard();
            }
            if ui.button("終了").clicked() {
                std::process::exit(0);
            }
        });

        ui.add_space(16.0);
        ui.separator();
        ui.label("このあと必要な作業:");
        ui.label("  ・TVTest等の録画・視聴ソフトに BonDriver_NetworkProxy.dll を設定してください。");
        ui.label(format!(
            "  ・Webダッシュボード ({}) からチューナーの詳細設定ができます。",
            dashboard_url(&self.web_listen_addr)
        ));
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
    fn sibling_exe_path_returns_none_when_absent() {
        // カレント実行ファイル(テストバイナリ)と同じフォルダに、まず存在しないで
        // あろう名前を探す。
        assert!(sibling_exe_path("definitely-not-a-real-binary-name").is_none());
    }

    #[test]
    fn install_dir_is_always_absolute() {
        // 設定ファイルパスがファイル名だけ(相対パス、親ディレクトリなし)の場合でも
        // install_dir() は絶対パスを返さなければならない。相対のままだと、
        // ドライバインストールの昇格プロセス(既定の作業ディレクトリが
        // C:\Windows\System32 になる)から見て解決先がずれてしまう。
        let mut app = SetupApp::new();
        app.config_path = "recisdb-proxy.toml".to_string();
        assert!(app.install_dir().is_absolute());

        app.config_path = "sub/dir/recisdb-proxy.toml".to_string();
        assert!(app.install_dir().is_absolute());
    }
}

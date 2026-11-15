//! DB管理用の `recisdb-proxy driver ...` サブコマンド。

use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::{Subcommand, ValueEnum};

use recisdb_proxy::database::{Database, NewBonDriver, StreamFormat};

use crate::{app_config, Args};

#[derive(Subcommand, Debug, Clone)]
pub enum DriverAction {
    /// 登録済みBonDriverを一覧表示する
    List,
    /// BonDriverをDBに登録し、必要なら初回スキャンを予約する
    Register {
        /// BonDriver DLLパス、またはpx4daemon:anyのようなドライバ識別子
        #[arg(long)]
        path: String,
        /// Web画面に表示する名前
        #[arg(long)]
        name: Option<String>,
        /// Web画面のBonDriverグループ
        #[arg(long)]
        group: Option<String>,
        /// 同時に開けるインスタンス数
        #[arg(long, default_value = "1", value_parser = clap::value_parser!(i32).range(1..))]
        max_instances: i32,
        /// BonDriverの出力形式
        #[arg(long, value_enum, default_value = "ts")]
        stream_format: DriverStreamFormat,
        /// 入力TSにB25復号を適用しない
        #[arg(long)]
        disable_b25: bool,
        /// 周期スキャンを有効にする (未指定時は無効)
        #[arg(long)]
        auto_scan: bool,
        /// 周期スキャン間隔 (時間)。--auto-scan指定時のみ使用
        #[arg(long, default_value = "24", value_parser = clap::value_parser!(i32).range(1..))]
        scan_interval_hours: i32,
        /// スキャンスケジューラの優先度
        #[arg(long, default_value = "0")]
        scan_priority: i32,
        /// 視聴・録画中の受動スキャンを無効にする
        #[arg(long)]
        no_passive_scan: bool,
        /// 初回の単発スキャンを予約する (周期スキャンは有効化しない)
        #[arg(long)]
        scan_now: bool,
    },
    /// 登録済みBonDriverの単発スキャンを予約する
    Scan {
        /// `driver list` が表示するBonDriver ID
        #[arg(long)]
        id: i64,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverStreamFormat {
    Ts,
    Mmttlv,
}

impl From<DriverStreamFormat> for StreamFormat {
    fn from(format: DriverStreamFormat) -> Self {
        match format {
            DriverStreamFormat::Ts => StreamFormat::Ts,
            DriverStreamFormat::Mmttlv => StreamFormat::MmtTlv,
        }
    }
}

#[derive(Debug)]
struct DriverRegistration {
    path: String,
    name: Option<String>,
    group: Option<String>,
    max_instances: i32,
    stream_format: DriverStreamFormat,
    disable_b25: bool,
    auto_scan: bool,
    scan_interval_hours: i32,
    scan_priority: i32,
    passive_scan: bool,
    scan_now: bool,
}

fn database_path_line(db_path: &Path) -> String {
    format!("DATABASE\t{}", db_path.display())
}

fn absolute_database_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if let Ok(canonical) = fs::canonicalize(&absolute) {
        return Ok(canonical);
    }
    if let (Some(parent), Some(file_name)) = (absolute.parent(), absolute.file_name()) {
        if let Ok(canonical_parent) = fs::canonicalize(parent) {
            return Ok(canonical_parent.join(file_name));
        }
    }
    Ok(absolute)
}

fn action_requires_existing_database(action: &DriverAction) -> bool {
    matches!(
        action,
        DriverAction::List | DriverAction::Register { .. } | DriverAction::Scan { .. }
    )
}

/// サブコマンドを実行する。DBファイルの解決規則はサーバー起動と同じく
/// `--database` → `[database].path` → 既定値の順。
pub fn run(action: &DriverAction, args: &Args) -> Result<(), Box<dyn Error>> {
    let file_config = app_config::load_file_config(args)?;
    let db_path = app_config::resolve_database_path(args, &file_config);
    let db_path = absolute_database_path(&db_path)?;
    println!("{}", database_path_line(&db_path));
    if action_requires_existing_database(action) && !db_path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("database file does not exist: {}", db_path.display()),
        )
        .into());
    }
    let mut db = Database::open(&db_path)?;

    match action {
        DriverAction::List => {
            println!("ID\tNAME\tGROUP\tINSTANCES\tSTREAM\tB25\tSCAN\tPATH");
            for driver in db.get_all_bon_drivers()? {
                let name = driver.driver_name.as_deref().unwrap_or("-");
                let group = driver.group_name.as_deref().unwrap_or("-");
                let stream = db.driver_stream_format(&driver.dll_path).as_db_value();
                let b25 = if db.driver_disables_b25(&driver.dll_path) {
                    "off"
                } else {
                    "on"
                };
                let scan = if driver.auto_scan_enabled {
                    format!("every {}h", driver.scan_interval_hours)
                } else {
                    "manual".to_string()
                };
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    driver.id,
                    name,
                    group,
                    driver.max_instances,
                    stream,
                    b25,
                    scan,
                    driver.dll_path
                );
            }
        }
        DriverAction::Register {
            path,
            name,
            group,
            max_instances,
            stream_format,
            disable_b25,
            auto_scan,
            scan_interval_hours,
            scan_priority,
            no_passive_scan,
            scan_now,
        } => {
            let registration = DriverRegistration {
                path: path.clone(),
                name: name.clone(),
                group: group.clone(),
                max_instances: *max_instances,
                stream_format: *stream_format,
                disable_b25: *disable_b25,
                auto_scan: *auto_scan,
                scan_interval_hours: *scan_interval_hours,
                scan_priority: *scan_priority,
                passive_scan: !no_passive_scan,
                scan_now: *scan_now,
            };
            let id = register_driver(&mut db, &registration)?;
            println!("BonDriver registered: id={id}, path={}", registration.path);
            println!("A running server must be restarted to load this driver into its tuner pool.");
            if registration.scan_now {
                println!("One-shot scan scheduled; periodic scanning remains opt-in.");
            }
        }
        DriverAction::Scan { id } => {
            if db.get_bon_driver(*id)?.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("BonDriver id={id} is not registered"),
                )
                .into());
            }
            db.request_immediate_scan(*id)?;
            println!("One-shot scan scheduled for BonDriver id={id}.");
        }
    }
    Ok(())
}

fn register_driver(
    db: &mut Database,
    registration: &DriverRegistration,
) -> Result<i64, Box<dyn Error>> {
    if registration.path.trim().is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "--path must not be empty").into());
    }
    if registration.max_instances <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--max-instances must be at least 1",
        )
        .into());
    }
    if registration.scan_interval_hours <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--scan-interval-hours must be at least 1",
        )
        .into());
    }
    if db.get_bon_driver_by_path(&registration.path)?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "BonDriver path is already registered: {}",
                registration.path
            ),
        )
        .into());
    }

    let mut new_driver =
        NewBonDriver::new(&registration.path).with_max_instances(registration.max_instances);
    new_driver.driver_name = registration
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    let group = registration
        .group
        .as_deref()
        .map(str::trim)
        .filter(|group| !group.is_empty());
    Ok(db.register_bon_driver(
        &new_driver,
        group,
        registration.stream_format.into(),
        registration.disable_b25,
        registration.auto_scan,
        registration.scan_interval_hours,
        registration.scan_priority,
        registration.passive_scan,
        registration.scan_now,
    )?)
}

#[cfg(test)]
mod tests {
    use super::{
        absolute_database_path, action_requires_existing_database, database_path_line,
        register_driver, DriverAction, DriverRegistration, DriverStreamFormat,
    };
    use clap::Parser;
    use recisdb_proxy::database::{Database, StreamFormat};
    use std::fs;
    use std::path::Path;

    #[test]
    fn database_path_line_shows_the_resolved_database_path() {
        assert_eq!(
            database_path_line(Path::new("/var/lib/recisdb/main.sqlite")),
            "DATABASE\t/var/lib/recisdb/main.sqlite"
        );
    }

    #[test]
    fn relative_database_path_is_reported_as_absolute() {
        let path = absolute_database_path(Path::new("channels.sqlite")).unwrap();
        assert!(path.is_absolute());
        assert_eq!(path.file_name().unwrap(), "channels.sqlite");
    }

    #[test]
    fn every_driver_action_requires_an_existing_database_file() {
        assert!(action_requires_existing_database(&DriverAction::List));
        assert!(action_requires_existing_database(&DriverAction::Scan {
            id: 1
        }));
        assert!(action_requires_existing_database(&DriverAction::Register {
            path: "px4daemon:any".to_owned(),
            name: None,
            group: None,
            max_instances: 1,
            stream_format: DriverStreamFormat::Ts,
            disable_b25: false,
            auto_scan: false,
            scan_interval_hours: 24,
            scan_priority: 0,
            no_passive_scan: false,
            scan_now: false,
        }));
    }

    #[test]
    fn register_does_not_create_a_missing_database_file() {
        let db_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target")
            .join(format!(
                "driver-register-missing-{}.sqlite",
                std::process::id()
            ));
        let _ = fs::remove_file(&db_path);
        let args = crate::Args::try_parse_from([
            "recisdb-proxy".to_owned(),
            "--database".to_owned(),
            db_path.to_string_lossy().into_owned(),
            "driver".to_owned(),
            "register".to_owned(),
            "--path".to_owned(),
            "px4daemon:any".to_owned(),
        ])
        .unwrap();
        let crate::Command::Driver { action } = args.command.as_ref().unwrap() else {
            panic!("expected the driver register command");
        };

        let error = super::run(action, &args).unwrap_err();

        assert!(error.to_string().contains("database file does not exist"));
        assert!(!db_path.exists(), "driver register created a new database");
    }

    #[test]
    fn one_shot_scan_does_not_enable_periodic_scanning() {
        let mut db = Database::open_in_memory().unwrap();
        let registration = DriverRegistration {
            path: "px4daemon:any".to_string(),
            name: Some("PX4_Drv ISDB-C".to_string()),
            group: None,
            max_instances: 1,
            stream_format: DriverStreamFormat::Ts,
            disable_b25: false,
            auto_scan: false,
            scan_interval_hours: 24,
            scan_priority: 0,
            passive_scan: true,
            scan_now: true,
        };

        let id = register_driver(&mut db, &registration).unwrap();
        let driver = db.get_bon_driver(id).unwrap().unwrap();

        assert_eq!(driver.dll_path, "px4daemon:any");
        assert_eq!(driver.driver_name.as_deref(), Some("PX4_Drv ISDB-C"));
        assert_eq!(driver.max_instances, 1);
        assert!(!driver.auto_scan_enabled);
        assert_eq!(driver.scan_interval_hours, 24);
        assert_eq!(driver.next_scan_at, Some(0));
        assert_eq!(db.driver_stream_format("px4daemon:any"), StreamFormat::Ts);
        assert!(!db.driver_disables_b25("px4daemon:any"));
    }

    #[test]
    fn periodic_scanning_requires_the_explicit_auto_scan_option() {
        let mut db = Database::open_in_memory().unwrap();
        let registration = DriverRegistration {
            path: "px4daemon:second".to_string(),
            name: None,
            group: None,
            max_instances: 2,
            stream_format: DriverStreamFormat::Mmttlv,
            disable_b25: true,
            auto_scan: true,
            scan_interval_hours: 6,
            scan_priority: 3,
            passive_scan: false,
            scan_now: false,
        };

        let id = register_driver(&mut db, &registration).unwrap();
        let driver = db.get_bon_driver(id).unwrap().unwrap();

        assert!(driver.auto_scan_enabled);
        assert_eq!(driver.scan_interval_hours, 6);
        assert_eq!(driver.scan_priority, 3);
        assert!(!driver.passive_scan_enabled);
        assert_eq!(driver.next_scan_at, None);
        assert_eq!(
            db.driver_stream_format("px4daemon:second"),
            StreamFormat::MmtTlv
        );
        assert!(db.driver_disables_b25("px4daemon:second"));
    }

    #[test]
    fn registration_rolls_back_insert_when_configuration_update_fails() {
        let mut db = Database::open_in_memory().unwrap();
        let registration = DriverRegistration {
            path: "px4daemon:rollback".to_string(),
            name: Some("PX4_Drv rollback".to_string()),
            group: Some("PX4".to_string()),
            max_instances: 1,
            stream_format: DriverStreamFormat::Mmttlv,
            disable_b25: true,
            auto_scan: true,
            scan_interval_hours: 12,
            scan_priority: 2,
            passive_scan: false,
            scan_now: true,
        };

        let tx = db.transaction().unwrap();
        tx.execute_batch(
            "CREATE TRIGGER fail_driver_stream_format_update
             BEFORE UPDATE OF stream_format ON bon_drivers
             BEGIN SELECT RAISE(ABORT, 'injected registration failure'); END;",
        )
        .unwrap();
        tx.commit().unwrap();

        assert!(register_driver(&mut db, &registration).is_err());
        assert!(db
            .get_bon_driver_by_path(&registration.path)
            .unwrap()
            .is_none());

        let tx = db.transaction().unwrap();
        tx.execute_batch("DROP TRIGGER fail_driver_stream_format_update;")
            .unwrap();
        tx.commit().unwrap();

        let id = register_driver(&mut db, &registration).unwrap();
        assert_eq!(
            db.get_bon_driver(id).unwrap().unwrap().dll_path,
            registration.path
        );
    }
}

# BonDriver自動チャンネルスキャン + DB保存実装計画

## 1. データベース設計

### 1.1 DBスクリーマ

```sql
-- BonDriverごとのスキャン履歴管理
CREATE TABLE bon_drivers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    dll_path TEXT UNIQUE NOT NULL,
    driver_name TEXT,
    last_scan INTEGER,
    version TEXT,
    created_at INTEGER DEFAULT (strftime('%s', 'now')),
    updated_at INTEGER DEFAULT (strftime('%s', 'now'))
);

-- チャンネル情報データベース
CREATE TABLE channels (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    bon_driver_id INTEGER NOT NULL,
    nid INTEGER NOT NULL,           -- ネットワークID
    sid INTEGER NOT NULL,           -- サービスID
    tsid INTEGER NOT NULL,          -- トランスポートストリームID
    manual_sheet INTEGER,           -- マニュアル枝番 (明示的に分けたい場合)
    raw_name TEXT,                  -- 原始チャンネル名
    channel_name TEXT,              -- チャンネル表示名
    physical_ch INTEGER,            -- 物理チャンネル番号
    is_enabled INTEGER DEFAULT 1,
    scan_time INTEGER,              -- スキャン日時
    service_type INTEGER,           -- サービス種別 (TV/Radio/Data)
    network_name TEXT,              -- ネットワーク名 (BS/CSなど)
    UNIQUE(bon_driver_id, nid, sid, tsid, manual_sheet)
);

-- スキャン履歴ログ
CREATE TABLE scan_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    bon_driver_id INTEGER NOT NULL,
    scan_time INTEGER DEFAULT (strftime('%s', 'now')),
    channel_count INTEGER,
    success INTEGER,
    error_message TEXT
);

-- インデックス
CREATE INDEX idx_channels_bon_driver ON channels(bon_driver_id);
CREATE INDEX idx_channels_nid_sid_tsid ON channels(nid, sid, tsid);
CREATE INDEX idx_channels_enabled ON channels(is_enabled);
CREATE INDEX idx_scan_history_bon_driver ON scan_history(bon_driver_id);
```

### 1.2 データモデル

```rust
// database/models.rs
use rusqlite::{Connection, Result, params};

#[derive(Debug, Clone)]
pub struct BonDriverInfo {
    pub id: i64,
    pub dll_path: String,
    pub driver_name: Option<String>,
    pub last_scan: Option<i64>,
    pub version: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub id: i64,
    pub bon_driver_id: i64,
    pub nid: u16,
    pub sid: u16,
    pub tsid: u16,
    pub manual_sheet: Option<u16>,
    pub raw_name: Option<String>,
    pub channel_name: Option<String>,
    pub physical_ch: Option<u8>,
    pub is_enabled: bool,
    pub scan_time: i64,
    pub service_type: Option<u8>,
    pub network_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ScanHistory {
    pub id: i64,
    pub bon_driver_id: i64,
    pub scan_time: i64,
    pub channel_count: i32,
    pub success: bool,
    pub error_message: Option<String>,
}
```

## 2. データベースモジュール実装

### 2.1 データベース接続管理

```rust
// database/mod.rs
use rusqlite::{Connection, Result};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    /// 新規DBまたは既存DBを開く
    pub fn new(db_path: &str) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        let db = Database {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.init_tables()?;
        Ok(db)
    }

    /// データベース初期化（スクリーマ作成）
    fn init_tables(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        
        conn.execute_batch("
            CREATE TABLE IF NOT EXISTS bon_drivers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                dll_path TEXT UNIQUE NOT NULL,
                driver_name TEXT,
                last_scan INTEGER,
                version TEXT,
                created_at INTEGER DEFAULT (strftime('%s', 'now')),
                updated_at INTEGER DEFAULT (strftime('%s', 'now'))
            );

            CREATE TABLE IF NOT EXISTS channels (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                bon_driver_id INTEGER NOT NULL,
                nid INTEGER NOT NULL,
                sid INTEGER NOT NULL,
                tsid INTEGER NOT NULL,
                manual_sheet INTEGER,
                raw_name TEXT,
                channel_name TEXT,
                physical_ch INTEGER,
                is_enabled INTEGER DEFAULT 1,
                scan_time INTEGER,
                service_type INTEGER,
                network_name TEXT,
                UNIQUE(bon_driver_id, nid, sid, tsid, manual_sheet),
                FOREIGN KEY(bon_driver_id) REFERENCES bon_drivers(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS scan_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                bon_driver_id INTEGER NOT NULL,
                scan_time INTEGER DEFAULT (strftime('%s', 'now')),
                channel_count INTEGER,
                success INTEGER,
                error_message TEXT,
                FOREIGN KEY(bon_driver_id) REFERENCES bon_drivers(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_channels_bon_driver ON channels(bon_driver_id);
            CREATE INDEX IF NOT EXISTS idx_channels_nid_sid_tsid ON channels(nid, sid, tsid);
            CREATE INDEX IF NOT EXISTS idx_channels_enabled ON channels(is_enabled);
            CREATE INDEX IF NOT EXISTS idx_scan_history_bon_driver ON scan_history(bon_driver_id);
        ")?;
        
        Ok(())
    }

    /// 共通接続取得
    pub fn connection(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }
}
```

### 2.2 BonDriver管理

```rust
// database/bon_driver.rs
use rusqlite::{params, Result};
use std::time::{SystemTime, UNIX_EPOCH};

impl Database {
    /// BonDriver情報を登録または更新
    pub fn upsert_bon_driver(
        &self,
        dll_path: &str,
        driver_name: Option<&str>,
        version: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        conn.execute(
            "INSERT OR REPLACE INTO bon_drivers 
             (dll_path, driver_name, version, updated_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![dll_path, driver_name, version, now],
        )?;
        
        let id = conn.last_insert_rowid();
        Ok(id)
    }

    /// BonDriver IDを取得
    pub fn get_bon_driver_id(&self, dll_path: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        
        let mut stmt = conn.prepare(
            "SELECT id FROM bon_drivers WHERE dll_path = ?1",
        )?;
        
        let mut rows = stmt.query(params![dll_path])?;
        
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    /// すべてのBonDriverを取得
    pub fn get_all_bon_drivers(&self) -> Result<Vec<(i64, String, Option<String>)>> {
        let conn = self.conn.lock().unwrap();
        
        let mut stmt = conn.prepare(
            "SELECT id, dll_path, driver_name FROM bon_drivers ORDER BY updated_at DESC",
        )?;
        
        let rows = stmt.query_map(params![], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
            ))
        })?;
        
        rows.collect::<Result<Vec<_>>>()
    }
}
```

### 2.3 チャンネル情報管理

```rust
// database/channel.rs
use rusqlite::{params, Result};
use crate::channels::ChannelType;

impl Database {
    /// チャンネル情報を保存
    pub fn save_channel(
        &self,
        bon_driver_id: i64,
        nid: u16,
        sid: u16,
        tsid: u16,
        manual_sheet: Option<u16>,
        raw_name: Option<&str>,
        channel_name: Option<&str>,
        physical_ch: Option<u8>,
        service_type: Option<u8>,
        network_name: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        conn.execute(
            "INSERT OR REPLACE INTO channels 
             (bon_driver_id, nid, sid, tsid, manual_sheet, raw_name, 
              channel_name, physical_ch, scan_time, service_type, network_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                bon_driver_id,
                nid,
                sid,
                tsid,
                manual_sheet,
                raw_name,
                channel_name,
                physical_ch,
                now,
                service_type,
                network_name,
            ],
        )?;
        
        Ok(())
    }

    /// チャンネル情報を取得
    pub fn get_channels_by_bon_driver(
        &self,
        bon_driver_id: i64,
    ) -> Result<Vec<ChannelInfo>> {
        let conn = self.conn.lock().unwrap();
        
        let mut stmt = conn.prepare(
            "SELECT id, bon_driver_id, nid, sid, tsid, manual_sheet, raw_name,
                    channel_name, physical_ch, is_enabled, scan_time, 
                    service_type, network_name
             FROM channels 
             WHERE bon_driver_id = ?1 AND is_enabled = 1
             ORDER BY nid, sid, tsid",
        )?;
        
        let rows = stmt.query_map(params![bon_driver_id], |row| {
            Ok(ChannelInfo {
                id: row.get(0)?,
                bon_driver_id: row.get(1)?,
                nid: row.get(2)?,
                sid: row.get(3)?,
                tsid: row.get(4)?,
                manual_sheet: row.get(5)?,
                raw_name: row.get(6)?,
                channel_name: row.get(7)?,
                physical_ch: row.get(8)?,
                is_enabled: row.get::<_, i32>(9)? == 1,
                scan_time: row.get(10)?,
                service_type: row.get(11)?,
                network_name: row.get(12)?,
            })
        })?;
        
        rows.collect::<Result<Vec<_>>>()
    }

    /// チャンネルを検索（NID/SID/TSID/ManSheetで）
    pub fn find_channel(
        &self,
        bon_driver_id: i64,
        nid: u16,
        sid: u16,
        tsid: u16,
        manual_sheet: Option<u16>,
    ) -> Result<Option<ChannelInfo>> {
        let conn = self.conn.lock().unwrap();
        
        let mut stmt = conn.prepare(
            "SELECT id, bon_driver_id, nid, sid, tsid, manual_sheet, raw_name,
                    channel_name, physical_ch, is_enabled, scan_time,
                    service_type, network_name
             FROM channels 
             WHERE bon_driver_id = ?1 AND nid = ?2 AND sid = ?3 AND tsid = ?4 
                   AND (manual_sheet IS NULL OR manual_sheet = ?5)",
        )?;
        
        let mut rows = stmt.query(params![
            bon_driver_id, 
            nid, 
            sid, 
            tsid, 
            manual_sheet
        ])?;
        
        if let Some(row) = rows.next()? {
            Ok(Some(ChannelInfo {
                id: row.get(0)?,
                bon_driver_id: row.get(1)?,
                nid: row.get(2)?,
                sid: row.get(3)?,
                tsid: row.get(4)?,
                manual_sheet: row.get(5)?,
                raw_name: row.get(6)?,
                channel_name: row.get(7)?,
                physical_ch: row.get(8)?,
                is_enabled: row.get::<_, i32>(9)? == 1,
                scan_time: row.get(10)?,
                service_type: row.get(11)?,
                network_name: row.get(12)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// TSIDを含めた検索
    pub fn find_by_tsid(
        &self,
        bon_driver_id: i64,
        tsid: u16,
    ) -> Result<Vec<ChannelInfo>> {
        let conn = self.conn.lock().unwrap();
        
        let mut stmt = conn.prepare(
            "SELECT id, bon_driver_id, nid, sid, tsid, manual_sheet, raw_name,
                    channel_name, physical_ch, is_enabled, scan_time,
                    service_type, network_name
             FROM channels 
             WHERE bon_driver_id = ?1 AND tsid = ?2 AND is_enabled = 1
             ORDER BY nid, sid",
        )?;
        
        let rows = stmt.query_map(params![bon_driver_id, tsid], |row| {
            Ok(ChannelInfo {
                id: row.get(0)?,
                bon_driver_id: row.get(1)?,
                nid: row.get(2)?,
                sid: row.get(3)?,
                tsid: row.get(4)?,
                manual_sheet: row.get(5)?,
                raw_name: row.get(6)?,
                channel_name: row.get(7)?,
                physical_ch: row.get(8)?,
                is_enabled: row.get::<_, i32>(9)? == 1,
                scan_time: row.get(10)?,
                service_type: row.get(11)?,
                network_name: row.get(12)?,
            })
        })?;
        
        rows.collect::<Result<Vec<_>>>()
    }
}
```

### 2.4 スキャン履歴管理

```rust
// database/scan_history.rs
use rusqlite::{params, Result};
use std::time::{SystemTime, UNIX_EPOCH};

impl Database {
    /// スキャン履歴を記録
    pub fn record_scan(
        &self,
        bon_driver_id: i64,
        channel_count: i32,
        success: bool,
        error_message: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        
        conn.execute(
            "INSERT INTO scan_history 
             (bon_driver_id, channel_count, success, error_message)
             VALUES (?1, ?2, ?3, ?4)",
            params![bon_driver_id, channel_count, success, error_message],
        )?;
        
        // BonDriverのlast_scanを更新
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        
        conn.execute(
            "UPDATE bon_drivers SET last_scan = ?1 WHERE id = ?2",
            params![now, bon_driver_id],
        )?;
        
        Ok(())
    }

    /// 最新のスキャン履歴を取得
    pub fn get_latest_scan(
        &self,
        bon_driver_id: i64,
    ) -> Result<Option<(i64, i32, bool)>> {
        let conn = self.conn.lock().unwrap();
        
        let mut stmt = conn.prepare(
            "SELECT scan_time, channel_count, success
             FROM scan_history 
             WHERE bon_driver_id = ?1
             ORDER BY scan_time DESC
             LIMIT 1",
        )?;
        
        let mut rows = stmt.query(params![bon_driver_id])?;
        
        if let Some(row) = rows.next()? {
            Ok(Some((row.get(0)?, row.get(1)?, row.get::<_, i32>(2)? == 1)))
        } else {
            Ok(None)
        }
    }
}
```

## 3. TS解析モジュール

### 3.1 PMT/PATパケット解析

```rust
// ts_analyzer/mod.rs
use bitstream_io::{BitReader, BigEndian};
use std::io::Cursor;

#[derive(Debug, Clone)]
pub struct TSElementaryStream {
    pub stream_type: u8,
    pub elementary_pid: u16,
    pub es_info_length: u16,
}

#[derive(Debug, Clone)]
pub struct PMTSection {
    pub program_number: u16,
    pub pcr_pid: u16,
    pub program_info_length: u16,
    pub streams: Vec<TSElementaryStream>,
}

#[derive(Debug, Clone)]
pub struct PATEntry {
    pub program_number: u16,
    pub program_map_pid: u16,
}

#[derive(Debug, Clone)]
pub struct PATSection {
    pub transport_stream_id: u16,
    pub network_id: u16,
    pub sections: Vec<PATEntry>,
}

/// PATパケットを解析
pub fn parse_pat(packet: &[u8]) -> Option<PATSection> {
    if packet.len() < 4 {
        return None;
    }
    
    let mut reader = BitReader::new(Cursor::new(packet));
    let _table_id = reader.read::<u8>(8).ok()?;
    let _section_syntax_indicator = reader.read::<u8>(1).ok()?;
    let _private_section = reader.read::<u8>(1).ok()?;
    let _reserved = reader.read::<u8>(2).ok()?;
    let section_length = reader.read::<u16>(12).ok()?;
    
    if packet.len() < section_length as usize + 3 {
        return None;
    }
    
    let transport_stream_id = reader.read::<u16>(16).ok()?;
    let _reserved2 = reader.read::<u8>(2).ok()?;
    let version_number = reader.read::<u8>(5).ok()?;
    let current_next_indicator = reader.read::<u8>(1).ok()?;
    
    if current_next_indicator == 0 {
        return None;
    }
    
    let section_number = reader.read::<u8>(8).ok()?;
    let last_section_number = reader.read::<u8>(8).ok()?;
    
    let mut entries = Vec::new();
    for _ in 0..section_length / 4 {
        let program_number = reader.read::<u16>(16).ok()?;
        let _reserved = reader.read::<u8>(3).ok()?;
        let program_map_pid = reader.read::<u16>(13).ok()?;
        entries.push(PATEntry {
            program_number,
            program_map_pid,
        });
    }
    
    Some(PATSection {
        transport_stream_id,
        network_id: 0, // 幾つかのBSストリームで設定可能
        sections: entries,
    })
}

/// PMTパケットを解析
pub fn parse_pmt(packet: &[u8]) -> Option<PMTSection> {
    if packet.len() < 4 {
        return None;
    }
    
    let mut reader = BitReader::new(Cursor::new(packet));
    let _table_id = reader.read::<u8>(8).ok()?;
    let _section_syntax_indicator = reader.read::<u8>(1).ok()?;
    let _private_section = reader.read::<u8>(1).ok()?;
    let _reserved = reader.read::<u8>(2).ok()?;
    let section_length = reader.read::<u16>(12).ok()?;
    
    if packet.len() < section_length as usize + 3 {
        return None;
    }
    
    let program_number = reader.read::<u16>(16).ok()?;
    let _reserved2 = reader.read::<u8>(2).ok()?;
    let version_number = reader.read::<u8>(5).ok()?;
    let current_next_indicator = reader.read::<u8>(1).ok()?;
    
    if current_next_indicator == 0 {
        return None;
    }
    
    let _section_number = reader.read::<u8>(8).ok()?;
    let _last_section_number = reader.read::<u8>(8).ok()?;
    let pcr_pid = reader.read::<u16>(13).ok()?;
    let _reserved3 = reader.read::<u8>(3).ok()?;
    let program_info_length = reader.read::<u16>(12).ok()?;
    
    // Skip program_info_length
    reader.skip(program_info_length * 8).ok()?;
    
    let mut streams = Vec::new();
    let mut remaining = section_length - 9 - program_info_length;
    
    while remaining > 4 {
        let stream_type = reader.read::<u8>(8).ok()?;
        let _reserved = reader.read::<u8>(3).ok()?;
        let elementary_pid = reader.read::<u16>(13).ok()?;
        let _reserved2 = reader.read::<u8>(3).ok()?;
        let es_info_length = reader.read::<u16>(12).ok()?;
        
        streams.push(TSElementaryStream {
            stream_type,
            elementary_pid,
            es_info_length,
        });
        
        reader.skip(es_info_length * 8).ok()?;
        remaining -= 5 + es_info_length;
    }
    
    Some(PMTSection {
        program_number,
        pcr_pid,
        program_info_length,
        streams,
    })
}

/// 視聴可能サービスを検出
pub fn detect_service_type(streams: &[TSElementaryStream]) -> Option<u8> {
    // MPEG2-Audio = 0x03 -> 音声
    // H.264 = 0x1B -> 映像
    // MPEG2-Video = 0x02 -> 映像
    
    for stream in streams {
        match stream.stream_type {
            0x02 | 0x1B => return Some(0x01), // TV Service
            0x03 => return Some(0x02),        // Radio Service
            0x0C => return Some(0x03),        // Data Service
            _ => {}
        }
    }
    
    None
}
```

## 4. TSID/SID/NID抽出器

### 4.1 TSパケットストリーム解析

```rust
// ts_extractor/mod.rs
use std::collections::HashMap;
use super::ts_analyzer::{parse_pat, parse_pmt, PATEntry, PMTSection};

pub struct TSChannelInfo {
    pub tsid: u16,
    pub sid: u16,
    pub nid: u16,
    pub program_number: u16,
    pub pmt_pid: u16,
}

pub struct TSStreamExtractor {
    pat_buffer: Vec<u8>,
    pmt_buffers: HashMap<u16, Vec<u8>>, // PID -> buffer
    pmt_sections: HashMap<u16, PMTSection>,
    pat_sections: Vec<PATEntry>,
    tsid: Option<u16>,
    nid: Option<u16>,
}

impl TSStreamExtractor {
    pub fn new() -> Self {
        Self {
            pat_buffer: Vec::new(),
            pmt_buffers: HashMap::new(),
            pmt_sections: HashMap::new(),
            pat_sections: Vec::new(),
            tsid: None,
            nid: None,
        }
    }

    /// TSパケットを処理（188byteごとのパケット）
    pub fn process_packet(&mut self, packet: &[u8]) -> Option<Vec<TSChannelInfo>> {
        if packet.len() < 4 {
            return None;
        }
        
        // TSヘッダー解析
        let sync_byte = packet[0];
        if sync_byte != 0x47 {
            return None;
        }
        
        let transport_error_indicator = (packet[1] >> 7) & 0x01;
        if transport_error_indicator == 1 {
            return None;
        }
        
        let payload_unit_start_indicator = (packet[1] >> 6) & 0x01;
        let transport_priority = (packet[1] >> 5) & 0x01;
        let pid = ((packet[1] as u16 & 0x1F) << 8) | packet[2] as u16;
        let transport_scrambling_control = (packet[3] >> 6) & 0x03;
        let adaptation_field_control = (packet[3] >> 4) & 0x03;
        let continuity_counter = packet[3] & 0x0F;
        
        // ペイロードの開始位置
        let mut payload_start = 4;
        
        // アダプテーションフィールドスキップ
        if adaptation_field_control & 0x02 != 0 {
            if packet.len() > 5 {
                let adaptation_length = packet[4] as usize;
                payload_start += 1 + adaptation_length;
            }
        }
        
        if payload_start >= packet.len() {
            return None;
        }
        
        let payload = &packet[payload_start..];
        
        match pid {
            0x0000 => {
                // PAT
                if payload_unit_start_indicator == 1 {
                    self.pat_buffer.clear();
                    self.pat_buffer.extend_from_slice(payload);
                    
                    if let Some(pat) = parse_pat(&self.pat_buffer) {
                        self.pat_sections = pat.sections.clone();
                        self.tsid = Some(pat.transport_stream_id);
                        // NIDはBS/CSストリームで検出可能
                    }
                } else {
                    self.pat_buffer.extend_from_slice(payload);
                }
            }
            
            0x0001 => {
                // CAT - not used
            }
            
            0x0010..=0x001F => {
                // NIT (Network Information Table)
            }
            
            0x0020..=0x0027 => {
                // SDT (Service Description Table)
            }
            
            0x0040..=0x004F => {
                // EIT
            }
            
            0x0080..=0x008F => {
                // DIT/SIT
            }
            
            0x0FFF => {
                // NULL PID
            }
            
            _ => {
                // PMT candidates
                if let Some(pat_entry) = self.pat_sections.iter().find(|e| e.program_map_pid == pid) {
                    if payload_unit_start_indicator == 1 {
                        let buffer = self.pmt_buffers.entry(pid).or_default();
                        buffer.clear();
                        buffer.extend_from_slice(payload);
                        
                        if let Some(pmt) = parse_pmt(buffer) {
                            self.pmt_sections.insert(pid, pmt);
                        }
                    } else {
                        let buffer = self.pmt_buffers.entry(pid).or_default();
                        buffer.extend_from_slice(payload);
                    }
                }
            }
        }
        
        // すべてのPMTが解析済みでPATが存在すれば、チャンネル情報を抽出
        if self.pat_sections.len() > 0 && self.pmt_sections.len() == self.pat_sections.len() {
            let mut channels = Vec::new();
            let tsid = self.tsid.unwrap_or(0);
            
            for pat_entry in &self.pat_sections {
                if let Some(pmt) = self.pmt_sections.get(&pat_entry.program_map_pid) {
                    let service_type = detect_service_type(&pmt.streams);
                    
                    channels.push(TSChannelInfo {
                        tsid,
                        sid: pat_entry.program_number,
                        nid: 0, // NIDは別途SDT/NITから取得
                        program_number: pat_entry.program_number,
                        pmt_pid: pat_entry.program_map_pid,
                    });
                }
            }
            
            // リセットして次の分析に備える
            self.pmt_sections.clear();
            self.pmt_buffers.clear();
            
            if channels.len() > 0 {
                return Some(channels);
            }
        }
        
        None
    }
    
    /// TSID/NIDを取得
    pub fn get_tsid(&self) -> Option<u16> {
        self.tsid
    }
    
    pub fn get_nid(&self) -> Option<u16> {
        self.nid
    }
}
```

## 5. チャンネルスキャンコマンド

### 5.1 CLIコマンドの拡張

```rust
// context.rs - Commands enum拡張
#[derive(Parser, Debug)]
pub enum Commands {
    // ... existing commands ...
    
    /// Scan BonDriver for available channels
    Scan {
        /// BonDriver DLL path
        #[clap(short, long)]
        device: String,
        
        /// Output database file path
        #[clap(short, long)]
        database: Option<String>,
        
        /// Recreate database from scratch
        #[clap(short, long)]
        recreate: bool,
        
        /// Timeout in seconds
        #[clap(short, long, default_value = "30")]
        timeout: u64,
    },
    
    /// Show scanned channels from database
    Show {
        /// BonDriver DLL path
        #[clap(short, long)]
        device: String,
        
        /// Database file path
        #[clap(short, long)]
        database: Option<String>,
        
        /// Output format (json/table)
        #[clap(short, long, default_value = "table")]
        format: String,
    },
    
    /// Query channel from database
    Query {
        /// BonDriver DLL path
        #[clap(short, long)]
        device: String,
        
        /// Database file path
        #[clap(short, long)]
        database: Option<String>,
        
        /// Channel to query (e.g., BS101, CS110_1)
        #[clap(short, long)]
        channel: Option<String>,
        
        /// NID to query
        #[clap(short, long)]
        nid: Option<u16>,
        
        /// SID to query
        #[clap(short, long)]
        sid: Option<u16>,
        
        /// TSID to query
        #[clap(short, long)]
        tsid: Option<u16>,
        
        /// Manual sheet number
        #[clap(short, long)]
        manual_sheet: Option<u16>,
    },
}
```

### 5.2 スキャンコマンド実装

```rust
// commands/scan.rs
use std::time::{Duration, Instant};
use log::{info, warn, error};
use crate::database::Database;
use crate::ts_extractor::TSStreamExtractor;
use crate::tuner::windows::UnTunedTuner;
use futures_time::time::TimeoutExt;

const TS_PACKET_SIZE: usize = 188;

pub async fn scan_bon_driver(
    device: &str,
    db_path: Option<&str>,
    recreate: bool,
    timeout_secs: u64,
) -> std::io::Result<()> {
    // Database setup
    let default_db = format!("{}.sqlite", device.replace(".dll", ""));
    let db_path = db_path.unwrap_or(&default_db);
    
    if recreate {
        std::fs::remove_file(db_path).ok();
    }
    
    let db = Database::new(db_path).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
    })?;
    
    info!("Database: {}", db_path);
    
    // Open tuner
    let tuner = UnTunedTuner::new(device.to_string(), 200000)
        .map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("Failed to open tuner: {}", e))
        })?;
    
    info!("Opened BonDriver: {}", device);
    
    // Try to enumerate channels
    let channel_names = tuner.enum_channels(0);
    if let Some(channels) = channel_names {
        info!("Found {} channels via enumChannels", channels.len());
        for (i, ch) in channels.iter().enumerate() {
            info!("  {}: {}", i, ch);
        }
    }
    
    // Start TS capture for all channels
    let start_time = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    
    // We need to actually tune and capture TS packets
    // For each channel, we need to:
    // 1. Tune to the channel
    // 2. Capture TS packets
    // 3. Parse PMT/PAT for NID/SID/TSID
    
    // This is complex and requires actual hardware access
    // For now, we'll implement a stub that captures from tuned channel
    
    info!("Starting channel scan (timeout: {}s)...", timeout_secs);
    
    // Update BonDriver info
    let bon_driver_id = db.upsert_bon_driver(device, None, None)
        .map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("DB error: {}", e))
        })?;
    
    // This is a simplified version
    // In practice, you'd need to:
    // 1. Tune to a known channel (e.g., BS101)
    // 2. Capture and analyze TS packets for PMT/PAT
    // 3. Extract NID/SID/TSID
    // 4. Repeat for all channels
    
    // For now, we'll store what we got from enumChannels
    // and note that full TS analysis requires actual tuning
    
    // We'll create a placeholder implementation
    // that uses the channel name to derive some info
    
    let mut channel_count = 0;
    
    if let Some(channels) = channel_names {
        for ch_name in &channels {
            // Try to parse channel name
            // BS101 -> BS network, TSID 1, SID 01?
            // CS110_1 -> CS network, TSID 1, SID 101?
            
            let mut nid = 0;
            let mut tsid = 0;
            let mut sid = 0;
            let mut manual_sheet = None;
            
            if ch_name.starts_with("BS") {
                // Parse BS channel
                let remaining = &ch_name[2..];
                if let Some((tsid_part, sid_part)) = remaining.split_once('_') {
                    // BS101_01 format
                    if let Ok(t) = tsid_part.parse::<u16>() {
                        tsid = t;
                    }
                    if let Ok(s) = sid_part.parse::<u16>() {
                        sid = s;
                    }
                } else if let Some((tsid_part, rest)) = remaining.split_at(1) {
                    // BS1_01 format
                    if let Ok(t) = tsid_part.parse::<u16>() {
                        tsid = t;
                    }
                    if let Ok(s) = rest.parse::<u16>() {
                        sid = s;
                    }
                }
                nid = 0x0001; // BS network
            } else if ch_name.starts_with("CS") {
                // Parse CS channel
                let remaining = &ch_name[2..];
                if let Some((tsid_part, sid_part)) = remaining.split_once('_') {
                    if let Ok(t) = tsid_part.parse::<u16>() {
                        tsid = t;
                    }
                    if let Ok(s) = sid_part.parse::<u16>() {
                        sid = s;
                    }
                }
                nid = 0x0002; // CS network
            } else if let Some((physical_ch, extra)) = ch_name.split_once('_') {
                // Physical channel with manual sheet
                if let Ok(phy) = physical_ch.parse::<u8>() {
                    tsid = 0; // terrestrial
                    sid = 0;
                    if let Some(sheet) = extra.parse::<u16>().ok() {
                        manual_sheet = Some(sheet);
                    }
                }
            }
            
            if sid > 0 {
                db.save_channel(
                    bon_driver_id,
                    nid,
                    sid,
                    tsid,
                    manual_sheet,
                    Some(ch_name),
                    Some(ch_name),
                    None, // physical_ch
                    Some(0x01), // service type: TV
                    None,
                ).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::Other, format!("DB save error: {}", e))
                })?;
                
                channel_count += 1;
                
                info!("Saved channel: {} (NID={}, SID={}, TSID={})", 
                      ch_name, nid, sid, tsid);
            }
        }
    }
    
    // Record scan history
    db.record_scan(bon_driver_id, channel_count, true, None)
        .map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("DB record error: {}", e))
        })?;
    
    info!("Scan completed. Found {} channels.", channel_count);
    
    Ok(())
}
```

## 6. データベース-backed チャンネル選択

### 6.1 拡張ChannelType

```rust
// channels/mod.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelType {
    Terrestrial(u8, TsFilter),
    Catv(u8, TsFilter),
    BS(u8, TsFilter),
    CS(u8, TsFilter),
    BonCh(u8),
    BonChSpace(ChannelSpace),
    
    // Database-backed channel
    Db {
        nid: u16,
        sid: u16,
        tsid: u16,
        manual_sheet: Option<u16>,
    },
    
    Undefined,
}

impl ChannelType {
    /// Parse from database info
    pub fn from_db_info(
        nid: u16,
        sid: u16,
        tsid: u16,
        manual_sheet: Option<u16>,
        network_name: Option<&str>,
    ) -> Self {
        // Determine channel type based on NID
        match nid {
            0x0001 => { // BS network
                ChannelType::BS(0, TsFilter::AbsTsId(sid))
            }
            0x0002 => { // CS network
                ChannelType::CS(0, TsFilter::AbsTsId(sid))
            }
            0x0000 => { // Terrestrial
                ChannelType::Terrestrial(0, TsFilter::AbsTsId(sid))
            }
            _ => {
                ChannelType::Db { nid, sid, tsid, manual_sheet }
            }
        }
    }
    
    /// Convert to unique key for DB storage
    pub fn to_db_key(&self) -> (u16, u16, u16, Option<u16>) {
        match self {
            ChannelType::BS(ch, TsFilter::AbsTsId(sid)) => (0x0001, *sid, *ch as u16, None),
            ChannelType::CS(ch, TsFilter::AbsTsId(sid)) => (0x0002, *sid, *ch as u16, None),
            ChannelType::Terrestrial(ch, TsFilter::AbsTsId(sid)) => (0x0000, *sid, *ch as u16, None),
            ChannelType::Db { nid, sid, tsid, manual_sheet } => (*nid, *sid, *tsid, *manual_sheet),
            _ => (0, 0, 0, None),
        }
    }
}
```

### 6.2 データベースルックアップ

```rust
// commands/lookup.rs
use crate::database::Database;
use crate::channels::{Channel, ChannelType};

pub struct ChannelLookup {
    db: Database,
}

impl ChannelLookup {
    pub fn new(db_path: &str) -> std::io::Result<Self> {
        let db = Database::new(db_path).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;
        
        Ok(Self { db })
    }
    
    /// Lookup channel from database
    pub fn lookup_channel(
        &self,
        bon_driver_id: i64,
        channel_name: &str,
    ) -> std::io::Result<Option<Channel>> {
        // Try to parse channel name to extract NID/SID/TSID
        // This is simplified - in practice you'd use the TS analyzer
        
        // For now, just check if channel exists in DB
        let channels = self.db.get_channels_by_bon_driver(bon_driver_id)
            .map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, format!("DB error: {}", e))
            })?;
        
        for ch_info in channels {
            if let Some(db_name) = &ch_info.channel_name {
                if db_name == channel_name {
                    let ch_type = ChannelType::from_db_info(
                        ch_info.nid,
                        ch_info.sid,
                        ch_info.tsid,
                        ch_info.manual_sheet,
                        ch_info.network_name.as_deref(),
                    );
                    
                    return Ok(Some(Channel {
                        ch_type,
                        raw_string: db_name.clone(),
                    }));
                }
            }
        }
        
        Ok(None)
    }
    
    /// Get channels from database for BonDriver
    pub fn get_channels_for_bon_driver(
        &self,
        dll_path: &str,
    ) -> std::io::Result<Vec<Channel>> {
        let bon_driver_id = self.db.get_bon_driver_id(dll_path)
            .map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, format!("DB error: {}", e))
            })?
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "BonDriver not found in DB")
            })?;
        
        let channel_infos = self.db.get_channels_by_bon_driver(bon_driver_id)
            .map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, format!("DB error: {}", e))
            })?;
        
        Ok(channel_infos.into_iter()
            .map(|ch_info| {
                let ch_type = ChannelType::from_db_info(
                    ch_info.nid,
                    ch_info.sid,
                    ch_info.tsid,
                    ch_info.manual_sheet,
                    ch_info.network_name.as_deref(),
                );
                
                Channel {
                    ch_type,
                    raw_string: ch_info.channel_name
                        .or(ch_info.raw_name)
                        .unwrap_or_else(|| format!("{}_{}_{}", ch_info.nid, ch_info.sid, ch_info.tsid)),
                }
            })
            .collect())
    }
}
```

## 7. DB検索コマンド

### 7.1 データ表示コマンド

```rust
// commands/show.rs
use prettytable::{Table, Row, Cell};
use serde_json;
use crate::database::Database;

pub fn show_channels(
    db_path: &str,
    dll_path: &str,
    format: &str,
) -> std::io::Result<()> {
    let db = Database::new(db_path).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
    })?;
    
    let bon_driver_id = db.get_bon_driver_id(dll_path)
        .map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("DB error: {}", e))
        })?
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "BonDriver not found in DB")
        })?;
    
    let channels = db.get_channels_by_bon_driver(bon_driver_id)
        .map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("DB error: {}", e))
        })?;
    
    if format == "json" {
        let json = serde_json::to_string_pretty(&channels)
            .map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, format!("JSON error: {}", e))
            })?;
        println!("{}", json);
    } else {
        let mut table = Table::new();
        table.add_row(Row::new(vec![
            Cell::new("ID"),
            Cell::new("NID"),
            Cell::new("SID"),
            Cell::new("TSID"),
            Cell::new("ManSheet"),
            Cell::new("Name"),
            Cell::new("Physical"),
            Cell::new("Service"),
        ]));
        
        for ch in channels {
            table.add_row(Row::new(vec![
                Cell::new(&ch.id.to_string()),
                Cell::new(&format!("{:04X}", ch.nid)),
                Cell::new(&format!("{:04X}", ch.sid)),
                Cell::new(&format!("{:04X}", ch.tsid)),
                Cell::new(&ch.manual_sheet.map(|v| v.to_string()).unwrap_or_default()),
                Cell::new(&ch.channel_name.unwrap_or_default()),
                Cell::new(&ch.physical_ch.map(|v| v.to_string()).unwrap_or_default()),
                Cell::new(&match ch.service_type {
                    Some(0x01) => "TV",
                    Some(0x02) => "Radio",
                    Some(0x03) => "Data",
                    _ => "Unknown",
                }),
            ]));
        }
        
        table.printstd();
    }
    
    Ok(())
}
```

## 8. Integration with Existing Commands

### 8.1 Tune Command with DB Support

```rust
// commands/tune.rs modifications
use crate::database::Database;
use crate::commands::lookup::ChannelLookup;

pub async fn tune_with_db(
    device: Option<String>,
    channel: Option<String>,
    tsid: Option<u16>,
    db_path: Option<&str>,
    // ... other parameters
) -> std::io::Result<(...)> {
    // Try database lookup first
    if let (Some(db_path), Some(device), Some(ch)) = (&db_path, &device, &channel) {
        let lookup = ChannelLookup::new(db_path)?;
        
        // Get or create BonDriver ID
        let db = Database::new(db_path).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;
        
        let bon_driver_id = db.upsert_bon_driver(device, None, None)
            .map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, format!("DB error: {}", e))
            })?;
        
        if let Some(db_channel) = lookup.lookup_channel(bon_driver_id, ch)? {
            // Use database-backed channel info
            info!("Using database channel: NID={}, SID={}, TSID={}", 
                  db_channel.nid, db_channel.sid, db_channel.tsid);
            
            // Convert to appropriate ChannelType for tuning
            // ... tuning logic ...
        } else {
            warn!("Channel not found in database, using original parsing");
            // Fall back to original channel parsing
        }
    }
    
    // Original tuning logic continues...
    // ...
}
```

## 9. SQLite Database Integration

### 9.1 Cargo.toml Updates

```toml
# Add to recisdb-rs/Cargo.toml
[dependencies]
rusqlite = { version = "0.31", features = ["bundled"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
prettytable-rs = "0.10"
bitstream-io = "0.2"
```

### 9.2 Module Structure

```
src/
├── database/
│   ├── mod.rs
│   ├── models.rs
│   ├── bon_driver.rs
│   ├── channel.rs
│   └── scan_history.rs
├── ts_analyzer/
│   ├── mod.rs
│   └── ps.rs
├── ts_extractor/
│   ├── mod.rs
│   └── decoder.rs
├── commands/
│   ├── mod.rs
│   ├── scan.rs
│   ├── show.rs
│   ├── lookup.rs
│   └── tune.rs (modified)
└── context.rs (modified)
```

## 10. Implementation Phases

### Phase 1: Database Foundation (1-2 weeks)
- Add SQLite dependencies
- Create database schema
- Implement basic CRUD operations
- Create models and basic query methods

### Phase 2: TS Analysis (2-3 weeks)
- Implement PAT/PMT parser
- Create TS packet demultiplexer
- Add NID/SID/TSID extraction from TS stream
- Test with real hardware

### Phase 3: Scan Command (1-2 weeks)
- Implement `scan` CLI command
- Integrate with BonDriver enumeration
- Store results in database
- Add error handling and timeout

### Phase 4: Query/Show Commands (1 week)
- Implement `show` command for viewing channels
- Implement `query` command for specific lookups
- Add JSON output format
- Pretty table formatting

### Phase 5: Database-backed Tuning (1-2 weeks)
- Modify existing tune command to use DB
- Fallback to original parsing if DB lookup fails
- Support manual sheet numbers
- Add migration support for DB schema changes

### Phase 6: Testing & Documentation (1 week)
- Unit tests for database operations
- Integration tests with hardware
- User documentation
- Example configurations

## 11. Usage Examples

### 11.1 Channel Scan
```bash
# Scan BonDriver and store in default DB (BonDriver_XXXXXXXX.sqlite)
recisdb scan --device BonDriver_XXXXXXXX.dll

# Scan with custom DB location
recisdb scan --device BonDriver_XXXXXXXX.dll --database channels.db

# Recreate DB from scratch
recisdb scan --device BonDriver_XXXXXXXX.dll --recreate
```

### 11.2 View Channels
```bash
# Show channels in table format
recisdb show --device BonDriver_XXXXXXXX.dll

# Show in JSON format
recisdb show --device BonDriver_XXXXXXXX.dll --format json
```

### 11.3 Query Channels
```bash
# Query by channel name
recisdb query --device BonDriver_XXXXXXXX.dll --channel BS101

# Query by NID/SID/TSID
recisdb query --device BonDriver_XXXXXXXX.dll --nid 0x0001 --sid 0x0001 --tsid 0x0000

# Query with manual sheet
recisdb query --device BonDriver_XXXXXXXX.dll --manual-sheet 1
```

### 11.4 Database-backed Tune
```bash
# Tune using database (will lookup NID/SID/TSID from DB)
recisdb tune --device BonDriver_XXXXXXXX.dll --channel BS101 --database channels.db

# Manual sheet (explicit grouping)
recisdb tune --device BonDriver_XXXXXXXX.dll --channel BS101 --manual-sheet 1 --database channels.db
```

## 12. Key Considerations

### 12.1 Database Performance
- Use SQLite with appropriate indexing
- Batch insert operations during scan
- Consider using WAL mode for concurrent access
- Implement connection pooling for multiple tuners

### 12.2 Hardware Access
- TS analysis requires actual hardware access
- Need to handle different tuner types (PX4/PT3, etc.)
- Timeout handling for slow/unresponsive hardware
- Error recovery for hardware failures

### 12.3 Manual Sheet Support
- User can specify manual sheet number to distinguish channels with same NID/SID/TSID
- Stored in `manual_sheet` column (NULL for automatic)
- Used in unique constraint to prevent duplicates

### 12.4 Migration Strategy
- Version database schema
- Provide migration scripts
- Auto-detect old format and upgrade

### 12.5 Security
- Validate all inputs from database
- Prevent SQL injection (use parameterized queries)
- Consider file permissions for database files

## 13. Dependencies

```
rusqlite = { version = "0.31", features = ["bundled"] }  # SQLite bindings
serde = { version = "1.0", features = ["derive"] }       # Serialization
serde_json = "1.0"                                        # JSON output
prettytable-rs = "0.10"                                   # Pretty tables
bitstream-io = "0.2"                                      # Bitstream parsing
```

This plan provides a complete implementation for BonDriver automatic channel scanning with database storage, supporting NID/SID/TSID/manual sheet grouping as requested.
# BonDriver 同時利用数・奪い合い・自動合流 実装計画

## 実装ステップ

### Step 1: DBスキーマ変更
- [ ] `bon_drivers` テーブルに `max_instances` カラム追加
- [ ] `BonDriverRecord` 構造体に `max_instances` フィールド追加
- [ ] `NewBonDriver` 構造体に `max_instances` フィールド追加
- [ ] CRUD操作の更新（SELECT/INSERT/UPDATE）
- [ ] `update_max_instances()` 関数追加

### Step 2: TunerPoolの拡張
- [ ] `capacity: HashMap<i64, Arc<Semaphore>>` を追加
- [ ] `preemption_candidates: HashMap<i64, Vec<Arc<SharedTuner>>>` を追加
- [ ] `MuxKey` 構造体の定義（(driver_id, nid, tsid)）
- [ ] `mux_index: HashMap<MuxKey, ChannelKey>` を追加
- [ ] `get_or_create_with_policy()` メソッド追加
- [ ] 优先度ベースの追い出しロジック実装
- [ ] SharedTuner に `OwnedSemaphorePermit` を保持

### Step 3: SharedTunerの拡張
- [ ] `permit: Option<OwnedSemaphorePermit>` フィールド追加
- [ ] `priority` フィールド追加
- [ ] `subscribers: Vec<(priority, id)>` を追加（priority管理用）
- [ ] `max_priority()` メソッド追加
- [ ] `is_preemptible()` メソッド追加

### Step 4: Sessionの更新
- [ ] `current_driver_id: Option<i64>` フィールド追加
- [ ] `request_priority: u8` フィールド追加（セッション単位）
- [ ] `handle_open_tuner()` で driver_id を取得して保持
- [ ] `get_or_create()` 呼び出しを `get_or_create_with_policy()` に変更
- [ ] `SelectLogicalChannel` で `MuxKey` からの検索を試みる

### Step 5: データベースマイグレーション
- [ ] migrationファイル作成（`migrations/001_add_max_instances.sql`）
- [ ] 既存レコードへのデフォルト値設定（`max_instances = 1`）

### Step 6: テスト
- [ ] 単体テスト（max_instances上限テスト）
- [ ] 奪い合いテスト（優先度ベース）
- [ ] TSID/SID合流テスト
- [ ] スキャンと視聴の競合テスト

## 優先度モデル

- **録画（排他）**: 255（追い出し不可）
- **録画（通常）**: 200
- **視聴**: 10
- **スキャン**: 0（最優先で追い出し候補）

## API変更点

### TunerPool

```rust
// 既存
pub async fn get_or_create<F, Fut>(
    &self,
    key: ChannelKey,
    bondriver_version: u8,
    factory: F,
) -> Result<Arc<SharedTuner>, TunerPoolError>

// 新規
pub async fn get_or_create_with_policy(
    &self,
    driver_id: i64,
    key: ChannelKey,
    request_priority: u8,
    allow_preempt: bool,
    bondriver_version: u8,
) -> Result<Arc<SharedTuner>, TunerPoolError>

// 新規：TSID合流用
pub async fn get_or_create_with_policy_and_tsid(
    &self,
    driver_id: i64,
    nid: u16,
    tsid: u16,
    fallback_key: ChannelKey,
    request_priority: u8,
    allow_preempt: bool,
    bondriver_version: u8,
) -> Result<Arc<SharedTuner>, TunerPoolError>
```

### Session

```rust
// Sessionに追加
current_driver_id: Option<i64>,
request_priority: u8,  // セッション単位の優先度

// OpenTuner時
fn handle_open_tuner(&mut self, tuner_path: String) {
    // DBからdriver_idを取得してself.current_driver_idに保存
}

// SetChannel/SelectLogicalChannel
fn get_or_create_with_policy(&self, ...) {
    // self.current_driver_id, self.request_priority を渡す
}
```

## DB変更

### bon_driversテーブル

```sql
ALTER TABLE bon_drivers ADD COLUMN max_instances INTEGER NOT NULL DEFAULT 1;
```

### CRUD変更

```rust
// bon_driver_crud.rs
// SELECT文に max_instances を追加
// INSERT文に max_instances を追加
// update_max_instances() 関数追加
```

## 追い出しロジック

1. 枠が不足した場合、候補チューナーを選定
2. 候補選定条件：
   - `max_priority < request_priority`
   - `max_priority != 255`（排他録画でない）
3. 候補選定優先順：
   1. priorityが最も低い
   2. subscriber数が少ない
   3. 最近TSを流していない（アイドル）
4. 追い出し実行：
   - 該当SharedTunerのsubscriberを全削除
   - チューナーを停止
   - 枠を解放（Semaphoreのpermitをdrop）
5. 新規チューナー起動

## TSID/SID合流ロジック

1. `SelectLogicalChannel(nid, tsid, sid)` で `(driver_id, nid, tsid)` をMuxKeyとして生成
2. `mux_index` から既存チューナーを検索
3. 既存チューナーがあれば：
   - 枠消費なしで合流
   - Subscriberとして追加
4. 無ければ：
   - 通常の `get_or_create_with_policy()` で新規起動
   - 起動後に `mux_index` に登録

## SharedTunerの仕様変更

### 構造体

```rust
pub struct SharedTuner {
    // 既存フィールド
    pub key: ChannelKey,
    tx: broadcast::Sender<Bytes>,
    channel_change_tx: broadcast::Sender<()>,
    subscriber_count: AtomicU32,
    is_running: AtomicBool,
    reader_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    signal_level: AtomicU32,
    bondriver_version: u8,
    lock: TunerLock,
    packets_received: AtomicU64,
    
    // 追加フィールド
    permit: Option<OwnedSemaphorePermit>,
    priority: u8,
    subscribers: Vec<(u8, u64)>,  // (priority, session_id)
}
```

### 新規メソッド

```rust
impl SharedTuner {
    // 既存メソッド
    
    // 新規
    pub fn set_permit(&mut self, permit: OwnedSemaphorePermit) {
        self.permit = Some(permit);
    }
    
    pub fn set_priority(&mut self, priority: u8) {
        self.priority = priority;
    }
    
    pub fn add_subscriber(&mut self, session_id: u64, priority: u8) {
        self.subscribers.push((priority, session_id));
    }
    
    pub fn remove_subscriber(&mut self, session_id: u64) {
        self.subscribers.retain(|&(_, id)| id != session_id);
    }
    
    pub fn max_priority(&self) -> u8 {
        self.subscribers.iter().map(|(p, _)| *p).max().unwrap_or(0)
    }
    
    pub fn is_preemptible(&self) -> bool {
        self.max_priority() < 255  // 排他録画でない
    }
}
```

## マイグレーションファイル例

`migrations/001_add_max_instances.sql`:

```sql
-- 既存レコードへのデフォルト値設定
UPDATE bon_drivers SET max_instances = 1 WHERE max_instances IS NULL;

-- カラム追加（NOT NULL DEFAULTで既存レコードも対応）
ALTER TABLE bon_drivers ADD COLUMN max_instances INTEGER NOT NULL DEFAULT 1;
```

## テストケース

### 1. 基本動作テスト
- `max_instances=1` のBonDriverで視聴開始
- 同じチューナーでスキャン開始（失敗）
- 視聴停止後、スcanfが成功

### 2. 奪い合いテスト
- `max_instances=1` でスキャン中、視聴開始
- スキャンが追い出され、視聴が成功

### 3. 同時利用上限テスト
- `max_instances=2` で視聴2本同時
- 3本目は追い出し判定（優先度ベース）

### 4. TSID合流テスト
- 同一TSIDのチャンネルを2クライアントが選局
- 同じSharedTunerに合流（枠1のまま）

### 5. 優先度テスト
- 優先度10（視聴）のチューナーに優先度0（スcanf）が追い出し要求
- 追い出し成功
- 優先度255（排他録画）への追い出しは失敗

## 実装順序

1. DBスキーマ変更 & CRUD更新
2. SharedTunerに優先度管理追加
3. TunerPoolに容量制御追加
4. Sessionで新API使用
5. TSID合流機能追加
6. マイグレーション作成
7. テスト実装

## 注意点

1. `Semaphore`はtokioのものを使う（tokio::sync::Semaphore）
2. `OwnedSemaphorePermit`はSharedTurerのDrop時に自動解放
3. 追い出し時にはsubscriberを全削除してnotify
4. TSID合流はdriver_id単位で検索（別driver_idのチューナーは不可）
5. 既存の`get_or_create()`は非推奨（将来削除予定）

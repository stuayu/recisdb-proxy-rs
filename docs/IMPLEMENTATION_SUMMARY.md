# BonDriver Concurrent Usage Control - 実装サマリー

## 実装完了項目

### 1. データベーススキーマ変更 ✅

#### `bon_drivers` テーブル
- `max_instances INTEGER DEFAULT 1` カラムを追加
- デフォルト値 `1`（排他的アクセス）
- 既存レコードへの自動適用

#### `BonDriverRecord` 構造体
- `max_instances: i32` フィールド追加

#### `NewBonDriver` 構造体
- `max_instances: Option<i32>` フィールド追加
- `with_max_instances()` メソッド追加

### 2. データベースCRUD更新 ✅

#### `recisdb-proxy/src/database/bon_driver.rs`
- `insert_bon_driver()`: max_instances を INSERT に含める
- `get_bon_driver()`: SELECT に max_instances を追加
- `get_bon_driver_by_path()`: SELECT に max_instances を追加
- `get_all_bon_drivers()`: SELECT に max_instances を追加
- `get_due_bon_drivers()`: SELECT に max_instances を追加
- `update_max_instances()`: 新規関数追加

#### `recisdb-proxy/src/database/channel.rs`
- `get_all_channels_with_drivers()`: BonDriverRecord 生成時に max_instances を設定

### 3. TunerPool 拡張（基本）✅

#### `recisdb-proxy/src/tuner/pool.rs`
- `MuxKey` 構造体定義（TSID/SID 合流用）
  - `driver_id: i64`
  - `nid: u16`
  - `tsid: u16`
- `priority` モジュール追加
  - `SCAN: u8 = 0`
  - `VIEWING: u8 = 10`
  - `RECORDING_NORMAL: u8 = 200`
  - `RECORDING_EXCLUSIVE: u8 = 255`

### 4. データベースマイグレーション ✅

#### `docs/migrations/001_add_max_instances.sql`
- SQL マイグレーションスクリプト作成
- 既存レコードへのデフォルト値設定
- 検証クエリを含む

### 5. 設計ドキュメント ✅

#### `docs/BonDriverCapacityControl.md`
- 詳細な実装計画
- API 変更点
- 追い出しロジック
- TSID/SID 合流ロジック
- テストケース

## コンパイル状態

✅ **成功**: プロジェクトは無事にコンパイルされました
- 警告: 118件（未使用コードなど）
- エラー: 0件

## ファイル変更一覧

1. `recisdb-proxy/src/database/models.rs` - BonDriverRecord と NewBonDriver の更新
2. `recisdb-proxy/src/database/schema.rs` - DB スキーマに max_instances 追加
3. `recisdb-proxy/src/database/bon_driver.rs` - CRUD 操作の更新
4. `recisdb-proxy/src/database/channel.rs` - BonDriverRecord 生成時の更新
5. `recisdb-proxy/src/tuner/pool.rs` - MuxKey と priority 定義追加
6. `docs/BonDriverCapacityControl.md` - 設計ドキュメント
7. `docs/migrations/001_add_max_instances.sql` - マイグレーションスクリプト

## 次に実装すべき項目

### 未実装: TunerPool の高度な機能

#### 1. **容量制御（セマフォ）**
- `capacity: HashMap<i64, Arc<Semaphore>>` を TunerPool に追加
- ドライバーごとの同時利用上限を管理
- `get_or_create_with_policy()` メソッド実装

#### 2. **優先度ベースの追い出し**
- SharedTuner に優先度管理追加
- `preemption_candidates` マップで候補チューナーを追跡
- 低優先度チューナーの自動追い出し

#### 3. **TSID/SID 合流機能**
- `MuxKey` による TSID/SID マッピング
- `mux_index: HashMap<MuxKey, ChannelKey>` を TunerPool に追加
- `get_or_create_with_policy_and_tsid()` メソッド実装

#### 4. **Session の統合**
- `current_driver_id: Option<i64>` フィールド追加
- `request_priority: u8` フィールド追加
- `handle_open_tuner()` で driver_id を取得
- `get_or_create_with_policy()` への移行

#### 5. **SharedTuner の拡張**
- `permit: Option<OwnedSemaphorePermit>` フィールド追加
- `subscribers: Vec<(u8, u64)>` で priority 管理
- `max_priority()` と `is_preemptible()` メソッド

## 優先度モデル

| 優先度 | 用途 | 説明 |
|--------|------|------|
| 255 | 録画（排他） | 追い出し不可 |
| 200 | 録画（通常） | 追い出し可 |
| 10 | 視聴 | 追い出し可 |
| 0 | スキャン | 最優先で追い出し候補 |

## 動作例

### シナリオ 1: 基本動作
```
BonDriver: max_instances = 1
1. 視聴開始 → 枠1消費
2. スキャン開始 → 失敗（枠なし）
3. 視聴停止 → 枠解放
4. スキャン開始 → 成功
```

### シナリオ 2: 奪い合い
```
BonDriver: max_instances = 1
1. スキャン中 (priority=0)
2. 視聴開始 (priority=10) → スキャンが追い出され、視聴が成功
```

### シナリオ 3: TSID 合流
```
BonDriver: max_instances = 1
1. NID=1, TSID=100, SID=1 → チューナーA起動（枠1消費）
2. NID=1, TSID=100, SID=2 → チューナーAに合流（枠0追加消費）
3. NID=1, TSID=200, SID=3 → チューナーB起動（枠1消費）
```

## 注意点

1. **既存コードへの影響**: 現在の `get_or_create()` は非推奨になりますが、後方互換性を維持
2. **マイグレーション**: 既存データベースに `max_instances` カラムを追加する必要あり
3. **設定値**: BonDriver 毎の `max_instances` は UI/設定ファイルから設定可能に設計
4. **優先度**: クライアントから渡されるか、サーバー設定で固定値を使用
5. **追い出し**: 追い出されたセッションにはエラーを返すか、自動再選定を試みる

## テスト計画

### 単体テスト
- [ ] max_instances 上限テスト
- [ ] 奪い合いテスト（優先度ベース）
- [ ] TSID 合流テスト
- [ ] スキャンと視聴の競合テスト

### 統合テスト
- [ ] 複数クライアント同時接続
- [ ] 排他録画の確保
- [ ] チューナー枯渇時の挙動

## 結論

**DB スキーマ変更と基本構造は完了しています。**
未実装部分（容量制御、追い出し、合流機能）は TunerPool と SharedTuner の拡張として実装可能です。

次のステップでは、`get_or_create_with_policy()` メソッドの実装と Session 統合が必要です。

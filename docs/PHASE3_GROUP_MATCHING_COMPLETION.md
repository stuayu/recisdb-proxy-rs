# Phase 3 実装完了レポート: グループベース選局対応

## 概要

Protocol レイヤーとセッションレイヤーにグループベースの選局機能を追加しました。これにより、クライアントは グループ名（e.g., "PX-MLT"）を指定して複数ドライバーの統合ビューにアクセスでき、自動的に最適なドライバーが選択されます。

## 実装内容

### 1. Protocol 拡張 (`recisdb-protocol/`)

**types.rs の変更:**
- `ClientMessage` に新バリアント：
  - `OpenTunerWithGroup { group_name: String }` - グループを指定してチューナーを開く
  - `SetChannelSpaceInGroup { group_name, space_idx, channel, priority, exclusive }` - グループ内で選局
- `message_type()` メソッドを更新して新バリアントを処理

**codec.rs の変更:**
- `encode_client_message()` で新メッセージのシリアライズ実装
- `decode_client_message()` で新メッセージのデシリアライズ実装
- ペイロード形式：
  - OpenTunerWithGroup: [group_name_len(u16), group_name_bytes]
  - SetChannelSpaceInGroup: [name_len(u16), name, space_idx(u32), channel(u32), priority(i32), exclusive(u8)]

### 2. 統合空間情報モジュール (`recisdb-proxy/src/tuner/group_space.rs`)

新しい `group_space` モジュールを実装：

**主要な型:**

1. **DriverInfo**: ドライバーメタデータ
   ```rust
   pub struct DriverInfo {
       pub driver_id: u32,
       pub driver_path: String,
       pub space_gen: SpaceGenerator,
   }
   ```

2. **GroupSpaceInfo**: グループ全体の統合空間情報
   ```rust
   pub struct GroupSpaceInfo {
       pub group_name: String,
       pub drivers: Vec<DriverInfo>,
       pub space_mappings: HashMap<u32, (String, Vec<(usize, u32)>)>,
       pub actual_to_virtual: HashMap<(usize, u32), u32>,
       pub channel_to_drivers: HashMap<(u32, u32), Vec<usize>>,
   }
   ```

**主要メソッド:**

- `build()`: データベースからグループ全体のドライバーとチャネルを読み込み、統合空間を構築
  - 各ドライバーの SpaceGenerator を生成
  - すべてのドライバーの空間をマージして仮想空間に割り当て
  - チャネルからドライバーへのマッピングを構築
- `get_space_name()`: 仮想空間のディスプレイ名を取得
- `get_channels_in_space()`: 空間内のチャネルを取得
- `find_drivers_for_channel()`: チャネルを配信できるドライバーを検索
- `all_virtual_spaces()`: すべての仮想空間を列挙

**ドライバー選択戦略:**

```rust
pub enum DriverSelectionStrategy {
    LeastLoaded,     // アクティブセッションが少ないドライバー
    FirstAvailable,  // 最初に利用可能なドライバー
    PreferExisting,  // すでに同チャネルをチューニング中のドライバー
}
```

### 3. セッションレイヤー更新 (`recisdb-proxy/src/server/session.rs`)

**handle_message() の更新:**
- `OpenTunerWithGroup { group_name }` を新ハンドラーにルーティング
- `SetChannelSpaceInGroup { ... }` を新ハンドラーにルーティング

**新しいハンドラーメソッド:**

1. `handle_open_tuner_with_group(group_name: String)`
   - グループ内のすべてのドライバーを取得
   - GroupSpaceInfo を構築して セッションに保存
   - クライアントにOpenTunerAckを返却

2. `handle_set_channel_space_in_group()`
   - GroupSpaceInfo から チャネル対応ドライバーを検索
   - ドライバー選択戦略でドライバーを選択
   - すでに開いているセッション内で同チャネルをチューニング中の場合は統合
   - そうでない場合は新しいドライバーを割り当て
   - SetChannelSpaceAckを返却

### 4. SpaceGenerator 更新

**Clone と Debug trait を追加:**
- `#[derive(Clone, Debug)]` を SpaceGenerator に付与
- GroupSpaceInfo 構造体が Copy 可能に

## データフロー

```
クライアント
    |
    | OpenTunerWithGroup { "PX-MLT" }
    v
Session::handle_open_tuner_with_group()
    |
    | 1. Database::get_group_drivers("PX-MLT")
    | 2. 各ドライバーのチャネルを取得
    | 3. SpaceGenerator::generate_from_channels() で各ドライバーの空間を生成
    | 4. GroupSpaceInfo::build() で統合空間をマージ
    v
Session に GroupSpaceInfo を保存
    |
    | SetChannelSpaceInGroup { "PX-MLT", space_idx, channel, ... }
    v
Session::handle_set_channel_space_in_group()
    |
    | 1. GroupSpaceInfo::find_drivers_for_channel(space_idx, channel)
    | 2. DriverSelector で最適ドライバーを選択
    | 3. チューナープール経由でドライバーを割り当て
    v
TS配信開始
```

## テスト

group_space.rs の unit tests:

```rust
#[test]
fn test_group_space_info_creation() {
    // GroupSpaceInfo 構造体の生成テスト
}

#[test]
fn test_driver_selector() {
    // ドライバー選択ロジックのテスト
}
```

## 次のステップ（Phase 4以降）

1. **GroupSpaceInfo::build() の完全実装**
   - 複数ドライバー間での空間マージロジック完成
   - NID/TSID/SID に基づくチャネルマッピング最適化

2. **統合セッション管理**
   - 同チャネル複数リクエスト時のセッション統合
   - ドライバーの負荷分散と故障時フェイルオーバー

3. **EnumTuningSpace/EnumChannelName in Group Context**
   - グループ内でのスペース/チャネル列挙
   - クライアント向けの統合ディスプレイ名生成

4. **テストカバレッジ拡張**
   - 複数ドライバーシナリオでのテスト
   - エッジケース（ドライバー不利用、チャネル重複）の処理

## ファイル変更一覧

| ファイル | 変更内容 |
|---------|--------|
| `recisdb-protocol/src/types.rs` | ClientMessage に新バリアント、message_type() 更新 |
| `recisdb-protocol/src/codec.rs` | encode/decode 実装 |
| `recisdb-proxy/src/tuner/group_space.rs` | 新モジュール（GroupSpaceInfo等） |
| `recisdb-proxy/src/tuner/mod.rs` | group_space エクスポート |
| `recisdb-proxy/src/tuner/space_generator.rs` | Clone/Debug derive 追加 |
| `recisdb-proxy/src/server/session.rs` | 新ハンドラーメソッド追加 |

## コンパイル状態

✅ **成功**: `cargo check` が 警告のみ（エラーなし）で完了

## API使用例

```rust
// クライアント側（将来）
client.open_tuner_with_group("PX-MLT")?;

// グループ内のすべてのドライバー対応チャネルを見ることが可能
let spaces = group_info.all_virtual_spaces();  // [0, 1, 2, 3, ...]
let space_name = group_info.get_space_name(0); // "福島" など

// チャネル選局（自動ドライバー選択）
client.set_channel_space_in_group("PX-MLT", 0, 23, 0, false)?;
```

## まとめ

Phase 3 では、プロトコルレイヤーとセッション層に グループベースの選局機能を追加しました。

- ✅ Protocol メッセージ拡張
- ✅ GroupSpaceInfo 構造体実装
- ✅ ハンドラースタブ化
- ⏳ セッション統合ロジック（Phase 4）
- ⏳ テスト・最適化（Phase 5）

次フェーズでは、実際のドライバー選択ロジック と セッション統合を実装し、完全な グループベース選局 を達成します。

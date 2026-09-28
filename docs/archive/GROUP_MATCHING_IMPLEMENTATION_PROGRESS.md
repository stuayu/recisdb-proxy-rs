# グループ名マッチングと自動チューナー空間生成 - 実装進捗

## 実装完了項目

### フェーズ1: BonDriverグループ統一名の導入 ✅ COMPLETED

#### 1.1 データベーススキーマ拡張
- **ファイル**: [recisdb-proxy/src/database/schema.rs](../../recisdb-proxy/src/database/schema.rs)
- 変更:
  - `bon_drivers` テーブに `group_name TEXT` カラムを追加
  - `channels` テーブに `band_type INTEGER` と `terrestrial_region TEXT` カラムを追加
  - インデックス: `idx_bon_drivers_group_name`, `idx_channels_band_type` を追加

#### 1.2 BonDriverRecord拡張
- **ファイル**: [recisdb-proxy/src/database/models.rs](../../recisdb-proxy/src/database/models.rs)
- 変更:
  - `BonDriverRecord` に `group_name: Option<String>` フィールドを追加
  - `ChannelRecord` に `band_type: Option<u8>` と `terrestrial_region: Option<String>` フィールドを追加

#### 1.3 グループ管理メソッドの実装
- **ファイル**: [recisdb-proxy/src/database/bon_driver.rs](../../recisdb-proxy/src/database/bon_driver.rs)
- 実装内容:
  - `get_group_drivers(group_name)`: グループ内の全ドライバーを取得
  - `set_group_name(id, group_name)`: ドライバーのグループ名を設定
  - `infer_group_name(dll_path)`: DLL名からグループ名を自動推測
    - `BonDriver_MLT1.dll` → `PX-MLT`
    - `BonDriver_PX-Q1UD.dll` → `PX-Q1UD`
    - `BonDriver_PX4-S.dll` → `PX4-S`

#### 1.4 SELECT クエリの更新
- **ファイル**: [recisdb-proxy/src/database/bon_driver.rs](../../recisdb-proxy/src/database/bon_driver.rs)
- 変更:
  - `get_bon_driver()`, `get_bon_driver_by_path()`, `get_all_bon_drivers()`, `get_due_bon_drivers()`
  - すべてのメソッドで `group_name` カラムを SELECT に追加

- **ファイル**: [recisdb-proxy/src/database/channel.rs](../../recisdb-proxy/src/database/channel.rs)
- 変更:
  - `row_to_channel_record()` で `band_type` と `terrestrial_region` を処理

---

### フェーズ2: チューナー空間自動生成ロジック ✅ COMPLETED

#### 2.1 BandType の実装
- **ファイル**: [recisdb-protocol/src/types.rs](../../recisdb-protocol/src/types.rs)
- 実装内容:
  - `BandType` enum: Terrestrial, BS, CS, FourK, Other
  - `BandType::from_nid(nid)`: NID から帯域を自動分類
  - `display_name()`: 日本語表示名（"地上波", "BS", "CS", "4K", "その他"）
  - `name_en()`: 英語表示名

- **ファイル**: [recisdb-protocol/src/lib.rs](../../recisdb-protocol/src/lib.rs)
- 変更:
  - `BandType` を pub use で export

#### 2.2 SpaceGenerator の実装
- **ファイル**: [recisdb-proxy/src/tuner/space_generator.rs](../../recisdb-proxy/src/tuner/space_generator.rs)（新規ファイル）
- 主な構成体:
  - `SpaceGenerator`: 仮想空間マッピング生成エンジン
  - `SpaceMapping`: 仮想空間 (virtual_space_idx) を実空間にマップ
  - `BandInfo`, `RegionInfo`: 帯域・地域情報

- コア機能:
  - `generate_from_channels(channels)`: チャンネル一覧から自動生成
    1. NID でグループ化
    2. 帯域分類
    3. 地デジ内の地域別細分化（地域A、地域B、BS、CS、その他）
    4. 存在しない帯域は自動スキップ
    5. 仮想空間を順序付け

  - `map_virtual_to_actual(virtual_space)`: 仮想 → 実空間マッピング
  - `enum_channels_in_space(virtual_space)`: 仮想空間内のチャンネル列挙
  - `get_virtual_spaces_for_actual(actual_space)`: 逆引き対応

- テスト:
  - `test_space_generator_empty()`: 空チャンネルの処理
  - `test_space_generator_single_terrestrial()`: 単一地デジ
  - `test_space_generator_mixed_bands()`: 複合帯域

- 地域推定:
  - `infer_region_from_nid(nid)`: NID から地域名を推測
  - 北海道、青森、岩手...沖縄の全都道府県対応

#### 2.3 モジュール統合
- **ファイル**: [recisdb-proxy/src/tuner/mod.rs](../../recisdb-proxy/src/tuner/mod.rs)
- 変更:
  - `pub mod space_generator;` を追加
  - `SpaceGenerator` と `SpaceMapping` を pub use で export

---

### フェーズ3: グループ内での選択ロジック 🚧 IN PROGRESS

#### 3.1 セッションでのグループサポート
- **ファイル**: [recisdb-proxy/src/server/session.rs](../../recisdb-proxy/src/server/session.rs)
- 計画:
  - `OpenTuner` メッセージにて `group_name` パラメータをサポート
  - グループ内のドライバーから空きを検索して自動選択
  - `space_idx` → `actual_space` マッピングの管理

#### 3.2 ドライバー別の空間マップキャッシング
- 計画:
  - `DriverSpaceInfo` 構造体の実装
  - 各ドライバーのチャンネルから `SpaceGenerator` を生成
  - セッション側でキャッシュ管理

---

## 次のステップ

### フェーズ3 の実装

1. **Session の `handle_open_tuner` 拡張**
   - `OpenTuner` メッセージに `group_name` フィールドを追加 (プロトコル層)
   - グループ指定時のドライバー自動選択

2. **DriverSpaceInfo の実装**
   ```rust
   pub struct DriverSpaceInfo {
       pub driver_path: String,
       pub space_generator: SpaceGenerator,
       pub actual_spaces: Vec<u32>,
   }
   ```
   - DB からチャンネル一覧を取得
   - 各ドライバーの `SpaceGenerator` を構築
   - キャッシュ機構

3. **Session の space_idx マッピング更新**
   - `map_space_idx_to_actual()` で `SpaceGenerator` を利用
   - グループ内でのマッピング統一

4. **SetChannelSpace のグループ対応**
   - グループ内の全ドライバーで同じ `space_idx` を解釈

---

## コンパイル状態

✅ **成功**: フェーズ1, 2 の全コード
```
warning: `recisdb-proxy` generated 136 warnings (apply 16 suggestions)
Finished `dev` profile [unoptimized + debuginfo]
```

---

## テスト状況

### 単体テスト

#### SpaceGenerator テスト
- [x] `test_space_generator_empty()`: 空チャンネルリストの処理
- [x] `test_space_generator_single_terrestrial()`: 単一地デジのマッピング
- [x] `test_space_generator_mixed_bands()`: 複合帯域（地デジ+BS+CS）の順序確認

### 統合テスト

- [ ] グループマッチング + チューナー選択
- [ ] 仮想空間マッピングの正確性
- [ ] グループ内でのチャンネル統一

---

## 実装の考慮点

### チャレンジ1: 複数DLLでの帯域・地域の不一致

**対応**:
- グループ内でも各ドライバーのチャンネル可用性は異なることを許容
- ドライバーの実際のチャンネル一覧から `SpaceGenerator` を個別生成
- `map_virtual_to_actual()` でベストエフォート対応

### チャレンジ2: NID からの地域推定

**対応**:
- `infer_region_from_nid()` で NID 値レンジから推定
- `ChannelRecord.terrestrial_region` で手動指定可能

### チャレンジ3: グループ内の空間インデックスの一貫性

**対応**:
- グループ単位で「カノニカルな空間割当」を定義
- グループ内全ドライバーの **和集合** チャンネルから空間を構築

---

## ファイル変更一覧

### スキーマ・モデル層
- [recisdb-proxy/src/database/schema.rs](../../recisdb-proxy/src/database/schema.rs): ✅
- [recisdb-proxy/src/database/models.rs](../../recisdb-proxy/src/database/models.rs): ✅
- [recisdb-proxy/src/database/bon_driver.rs](../../recisdb-proxy/src/database/bon_driver.rs): ✅
- [recisdb-proxy/src/database/channel.rs](../../recisdb-proxy/src/database/channel.rs): ✅

### プロトコル層
- [recisdb-protocol/src/types.rs](../../recisdb-protocol/src/types.rs): ✅
- [recisdb-protocol/src/lib.rs](../../recisdb-protocol/src/lib.rs): ✅

### ロジック層
- [recisdb-proxy/src/tuner/space_generator.rs](../../recisdb-proxy/src/tuner/space_generator.rs): ✅ NEW
- [recisdb-proxy/src/tuner/mod.rs](../../recisdb-proxy/src/tuner/mod.rs): ✅

### セッション層 (進行中)
- [recisdb-proxy/src/server/session.rs](../../recisdb-proxy/src/server/session.rs): 🚧

---

## 関連ドキュメント

- [GROUP_MATCHING_IMPLEMENTATION.md](./GROUP_MATCHING_IMPLEMENTATION.md): 実装計画
- [ARCHITECTURE.md](./ARCHITECTURE.md): 全体アーキテクチャ

# クライアントグループマッチングとチューナー空間自動生成 - 実装概要

## 要件のマッピング

ユーザーの要件:
> クライアントからグループ名を指定してサーバー側で一致するグループからチャンネルを選択したい。例）クライアント（PX-MLT）→ サーバー(MLT1.dll,MLT2.dll,PX-Q1UD.dll)から自動選択。

> チューナー空間自動生成処理で0は地デジ・1はBS・2はCS・3は4K・5はその他としたい。※存在しない帯域は前に詰める

> 地デジ内部に複数の地域が混在する場合は地域名でチューナー空間を生成してください。

> 同一グループ内のspace channelはすべて一致するとは限らないし、グループ内部で選局できるチャンネルが限られている場合もある。

## 実装内容

### 1. グループ名管理

#### スキーマ
```sql
-- bon_drivers テーブに追加
ALTER TABLE bon_drivers ADD COLUMN group_name TEXT;
```

#### グループ名の自動推測
DLL ファイル名から自動生成:
- `BonDriver_MLT1.dll` → `PX-MLT`
- `BonDriver_MLT2.dll` → `PX-MLT`
- `BonDriver_PX-Q1UD.dll` → `PX-Q1UD`
- `BonDriver_PX4-S.dll` → `PX4-S`

#### データベースメソッド
```rust
// recisdb-proxy/src/database/bon_driver.rs
pub fn get_group_drivers(&self, group_name: &str) -> Result<Vec<BonDriverRecord>>;
pub fn set_group_name(&self, id: i64, group_name: Option<&str>) -> Result<()>;
pub fn infer_group_name(dll_path: &str) -> Option<String>;
```

---

### 2. 帯域分類とチューナー空間自動生成

#### BandType の追加
```rust
// recisdb-protocol/src/types.rs
pub enum BandType {
    Terrestrial = 0,  // 地デジ
    BS = 1,           // BS衛星
    CS = 2,           // CS衛星
    FourK = 3,        // 4K衛星
    Other = 4,        // その他
}

impl BandType {
    pub fn from_nid(nid: u16) -> Self { /* NID から自動分類 */ }
    pub fn display_name(&self) -> &'static str;  // 日本語表示
}
```

#### スキーマ拡張
```sql
-- channels テーブに追加
ALTER TABLE channels ADD COLUMN band_type INTEGER;         -- 0-4
ALTER TABLE channels ADD COLUMN terrestrial_region TEXT;   -- "地域A", "地域B" など
```

#### SpaceGenerator による自動生成
```rust
// recisdb-proxy/src/tuner/space_generator.rs
pub struct SpaceGenerator {
    mappings: Vec<SpaceMapping>,
    actual_to_virtual: HashMap<u32, Vec<u32>>,
}

pub struct SpaceMapping {
    pub virtual_space: u32,          // 仮想空間インデックス (0, 1, 2, ...)
    pub display_name: String,        // "地域A", "地域B", "BS", "CS"
    pub band_type: BandType,
    pub region_name: Option<String>, // 地デジのみ
    pub actual_spaces: Vec<u32>,     // 実ボンドライバー空間番号
}
```

**生成アルゴリズム**:
1. チャンネルを NID で分類
2. NID から帯域分類 (`BandType::from_nid()`)
3. 地デジの場合、さらに地域別に細分化
4. 帯域順 (地デジ → BS → CS → 4K → その他) で仮想空間を割当
5. 存在しない帯域は自動スキップ

**例**:
```
チャンネル一覧:
- NID=0x7FE0, bon_space=0   → 地域A (band=Terrestrial)
- NID=0x7FE4, bon_space=0   → 地域B (band=Terrestrial)
- NID=0x4011, bon_space=1   → BS (band=BS)
- NID=0x6001, bon_space=2   → CS (band=CS)

生成結果:
- virtual_space=0: 地域A 地上波
- virtual_space=1: 地域B 地上波
- virtual_space=2: BS衛星
- virtual_space=3: CS衛星
```

#### 地域推定
NID 値レンジから地域を自動推測:
```rust
fn infer_region_from_nid(nid: u16) -> String {
    match nid {
        0x7F80..=0x7F8F => "北海道",
        0x7F50..=0x7F5F => "地域B",
        0x7F20..=0x7F2F => "地域A",
        0x7F00..=0x7F0F => "神奈川",
        ...
    }
}
```

---

### 3. グループ内の選択と空間マッピング

#### 複数ドライバーでの対応

**要件**: グループ内のドライバーは同じチャンネルを必ずしも提供しない

**対応方法**:
1. グループ内の各ドライバーに対して個別の `SpaceGenerator` を生成
   - MLT1.dll: 地域A地上波のみ → virtual_space={0: 地域A}
   - MLT2.dll: 地域A+地域B地上波 → virtual_space={0: 地域A, 1: 地域B}
   - MLT3.dll: 地域A+地域B+BS → virtual_space={0: 地域A, 1: 地域B, 2: BS}

2. クライアント側から `space_idx` が指定されたとき:
   - グループ内のドライバーを順に調査
   - その `space_idx` に対応するチャンネルを持つドライバーを選択
   - 複数該当する場合は、負荷の低いものを選択 (スコアベース)

#### 実装予定

**Session 拡張** ([recisdb-proxy/src/server/session.rs](../../recisdb-proxy/src/server/session.rs)):
```rust
pub struct Session {
    // 既存...
    /// ドライバーごとの空間ジェネレータキャッシュ
    space_generators: HashMap<String, SpaceGenerator>,
}

impl Session {
    /// グループ内でドライバーを自動選択
    async fn select_tuner_from_group(
        &mut self,
        group_name: &str,
    ) -> Result<(Arc<SharedTuner>, String)>;

    /// 仮想 space_idx から実ドライバー空間へマップ
    async fn map_space_idx_to_driver_space(
        &mut self,
        driver_path: &str,
        space_idx: u32,
    ) -> Result<u32>;
}
```

---

## 設計パターン

### ベストエフォート方式

グループ内でも各ドライバーのサポート状況が異なることを前提に設計:

| ドライバー | virtual_space=0 | virtual_space=1 | virtual_space=2 |
|-----------|-----------------|-----------------|-----------------|
| MLT1.dll  | ✅ 地域A        | ❌ (スキップ)   | ❌              |
| MLT2.dll  | ✅ 地域A        | ✅ 地域B        | ❌              |
| MLT3.dll  | ✅ 地域A        | ✅ 地域B        | ✅ BS           |

クライアントが `space_idx=1` (地域B) をリクエスト:
1. MLT1.dll: ❌ 地域Bチャンネルなし
2. MLT2.dll: ✅ 地域Bチャンネルあり → 選択
3. MLT3.dll: ✅ 但し優先度は MLT2 より低い

---

## 実装状況

### フェーズ1: グループ名管理 ✅

- [x] スキーマ拡張
- [x] Models 更新
- [x] グループマッピング機能
- [x] 自動推測機能
- [x] SELECT クエリ更新

### フェーズ2: 帯域分類と空間生成 ✅

- [x] `BandType` 実装
- [x] NID 分類ロジック
- [x] `SpaceGenerator` 実装
- [x] 地域推定ロジック
- [x] テスト実装

### フェーズ3: グループ内選択 🚧

- [ ] Session 拡張
- [ ] DriverSpaceInfo 実装
- [ ] ドライバー選択ロジック
- [ ] space_idx マッピング

### フェーズ4: テスト・検証 ⏳

- [ ] 単体テスト
- [ ] 統合テスト

---

## 関連ファイル

### スキーマ・モデル
- [recisdb-proxy/src/database/schema.rs](../../recisdb-proxy/src/database/schema.rs)
- [recisdb-proxy/src/database/models.rs](../../recisdb-proxy/src/database/models.rs)
- [recisdb-proxy/src/database/bon_driver.rs](../../recisdb-proxy/src/database/bon_driver.rs)
- [recisdb-proxy/src/database/channel.rs](../../recisdb-proxy/src/database/channel.rs)

### プロトコル・ロジック
- [recisdb-protocol/src/types.rs](../../recisdb-protocol/src/types.rs)
- [recisdb-protocol/src/lib.rs](../../recisdb-protocol/src/lib.rs)
- [recisdb-proxy/src/tuner/space_generator.rs](../../recisdb-proxy/src/tuner/space_generator.rs)
- [recisdb-proxy/src/tuner/mod.rs](../../recisdb-proxy/src/tuner/mod.rs)

### 統計・詳細
- [docs/GROUP_MATCHING_IMPLEMENTATION.md](./GROUP_MATCHING_IMPLEMENTATION.md)
- [docs/GROUP_MATCHING_IMPLEMENTATION_PROGRESS.md](./GROUP_MATCHING_IMPLEMENTATION_PROGRESS.md)

# グループ名マッチングと自動チューナー空間生成の実装計画

## 概要
クライアント側でグループ名を指定し、サーバー側で一致するグループ（BonDriverの一覧）から自動的にチャンネルを選択するシステムを実装。同時に、帯域別・地域別の自動チューナー空間生成ロジックも追加。

## 現在の構造

### 1. BonDriver管理
- `recisdb-proxy/src/database/bon_driver.rs`: BonDriver記録管理
- `recisdb-proxy/src/bondriver/windows.rs`: BonDriver FFI実装
- 複数のDLLパス（MLT1.dll, MLT2.dll等）を個別に管理

### 2. チャンネル管理
- `recisdb-proxy/src/database/models.rs`: ChannelRecord
  - `bon_driver_id`: DLLごとのID
  - `bon_space`: チューナー空間（0,1,2,3...）
  - `bon_channel`: チャンネル番号
  - `nid, sid, tsid`: 論理チャンネル識別子

### 3. チューナー空間（space）の現状
- `recisdb-proxy/src/server/session.rs`: space_idx → actual_space マッピング
  - 現在は単純なインデックスマッピング
  - チャンネルの帯域情報を活用していない

### 4. プロトコル
- `recisdb-protocol/src/types.rs`: ClientMessage, ChannelSelector
- `recisdb-protocol/src/broadcast_region.rs`: NID分類

---

## 実装内容

### フェーズ1: BonDriverグループ統一名の導入

#### 1.1 データベーススキーマ拡張
**ファイル**: `recisdb-proxy/src/database/schema.rs`

```sql
-- bon_drivers テーブに追加
ALTER TABLE bon_drivers ADD COLUMN group_name TEXT;  -- 例: "PX-MLT", "PX-Q1UD"
ALTER TABLE bon_drivers ADD UNIQUE(group_name);
```

#### 1.2 BonDriverRecord拡張
**ファイル**: `recisdb-proxy/src/database/models.rs`

```rust
pub struct BonDriverRecord {
    // ... 既存フィールド
    pub group_name: Option<String>,  // グループ統一名
}
```

#### 1.3 グループ内のDLL自動検出
**新規ファイル**: `recisdb-proxy/src/database/group.rs`

```rust
/// グループ内の利用可能なBonDriver一覧を取得
pub fn get_group_drivers(&self, group_name: &str) -> Result<Vec<BonDriverRecord>> {
    // group_name で複数の BonDriverRecord を取得
}

/// DLL名からグループ名を推測（手動設定 or 自動検出）
pub fn infer_group_name(dll_path: &str) -> Option<String> {
    // "BonDriver_MLT1.dll" → "PX-MLT"
    // "BonDriver_MLT2.dll" → "PX-MLT"
    // "BonDriver_PX-Q1UD.dll" → "PX-Q1UD"
}
```

#### 1.4 セッションでのグループマッチング
**ファイル**: `recisdb-proxy/src/server/session.rs` - OpenTuner処理

```rust
async fn handle_open_tuner(&mut self, group_name: Option<String>) -> Result<()> {
    // グループ名が指定されている場合
    if let Some(group) = group_name {
        let db = self.database.lock().await;
        let drivers = db.get_group_drivers(&group)?;
        
        // グループ内のドライバーから空いているものを順に選択
        for driver in drivers {
            if try_lock_tuner(&driver.dll_path).await? {
                self.current_tuner_path = Some(driver.dll_path);
                break;
            }
        }
    }
}
```

---

### フェーズ2: チューナー空間自動生成ロジック

#### 2.1 帯域分類の強化
**ファイル**: `recisdb-protocol/src/broadcast_region.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandType {
    Terrestrial,  // 地デジ (0で始まる)
    BS,           // BS衛星 (1で始まる)
    CS,           // CS衛星 (2で始まる)
    FourK,        // 4K衛星 (3で始まる)
    Other,        // その他 (4で始まる)
}

/// NIDから帯域を判定
pub fn nid_to_band(nid: u16) -> BandType {
    match (nid >> 12) & 0xF {
        0 => BandType::Terrestrial,
        1 => BandType::BS,
        2 => BandType::CS,
        3 => BandType::FourK,
        _ => BandType::Other,
    }
}
```

#### 2.2 地域別空間生成
**新規ファイル**: `recisdb-proxy/src/tuner/space_generator.rs`

```rust
/// チューナーの全チャンネルから仮想空間マップを生成
pub struct SpaceGenerator {
    /// 帯域ごとの構成
    bands: Vec<BandInfo>,
}

pub struct BandInfo {
    pub band_type: BandType,
    pub regions: Vec<RegionInfo>,  // 地デジの場合のみ複数
}

pub struct RegionInfo {
    pub region_name: String,
    pub nids: Vec<u16>,
}

impl SpaceGenerator {
    /// チューナーの全チャンネルから仮想空間を生成
    pub fn generate_from_channels(
        channels: &[ChannelRecord],
    ) -> Result<Self> {
        // 1. チャンネルをNIDで分類
        // 2. 各NIDを帯域分類
        // 3. 地デジ内の複数NIDは地域名でグループ化
        // 4. 存在しない帯域は詰める
        
        // 例出力:
        // Space 0: 福島地上波 (NID 0x7FE0-0x7FE1)
        // Space 1: 宮城地上波 (NID 0x7FE2-0x7FE3)
        // Space 2: BS衛星 (NID 0x4011-0x4013)
        // Space 3: CS衛星 (NID 0x4041-0x404D)
        // Space 4: その他
    }
    
    /// 仮想space_idx → 実bon_spaceマッピング
    pub fn map_virtual_to_actual(
        &self,
        virtual_space: u32,
        bon_spaces_available: &[u32],
    ) -> Result<u32> {
        // virtual_space に対応する実bon_spaceを返す
    }
}
```

#### 2.3 BonDriver個別の空間マッピング
**新規ファイル**: `recisdb-proxy/src/tuner/driver_space_info.rs`

```rust
/// BonDriver個別の空間情報
pub struct DriverSpaceInfo {
    pub driver_path: String,
    pub space_generator: SpaceGenerator,
    pub actual_spaces: Vec<u32>,  // BonDriverで実際に使用可能な空間番号
}

impl DriverSpaceInfo {
    /// ドライバーの全チャンネルからGeneratorを生成
    pub async fn build_from_db(
        db: &Database,
        driver_path: &str,
    ) -> Result<Self> {
        let channels = db.get_channels_by_bon_driver(driver_id)?;
        let generator = SpaceGenerator::generate_from_channels(&channels)?;
        let actual_spaces = extract_actual_spaces(&channels);
        
        Ok(Self {
            driver_path: driver_path.to_string(),
            space_generator: generator,
            actual_spaces,
        })
    }
}
```

---

### フェーズ3: グループ内での選択ロジック

#### 3.1 グループ内の同一チャンネル選択
**ファイル**: `recisdb-proxy/src/tuner/selector.rs` 拡張

```rust
/// グループ内のドライバーから、指定チャンネルをサポートするものを探す
pub async fn select_from_group(
    &self,
    group_name: &str,
    space_idx: u32,           // 仮想空間インデックス
    channel: u32,             // 仮想チャンネル
) -> Result<(Arc<SharedTuner>, String, DriverSpaceInfo)> {
    // 1. グループ内の全ドライバーを取得
    let drivers = db.get_group_drivers(group_name)?;
    
    // 2. 各ドライバーについて:
    //    - 空間ジェネレータから space_idx に対応する bon_space を計算
    //    - そのbon_spaceでchannelが存在するか確認
    //    - スコア（負荷、優先度）で最良のドライバーを選択
    
    for driver in drivers {
        let space_info = DriverSpaceInfo::build_from_db(&db, &driver.dll_path).await?;
        if let Ok(actual_space) = space_info.space_generator.map_virtual_to_actual(
            space_idx,
            &space_info.actual_spaces,
        ) {
            // actual_space での channel の可用性を確認
            if let Some(ch_entry) = find_channel(&db, &driver.dll_path, actual_space, channel) {
                // スコア計算して候補に追加
            }
        }
    }
    
    // スコア最高のドライバーを返す
}
```

#### 3.2 グループ内のチャンネル選択肢の統一
**ファイル**: `recisdb-proxy/src/server/session.rs`

```rust
/// グループ内の共通チャンネル一覧を列挙
async fn handle_enum_channel_name_group(
    &mut self,
    group_name: &str,
    space_idx: u32,
    channel: u32,
) -> Result<Vec<String>> {
    let db = self.database.lock().await;
    let drivers = db.get_group_drivers(group_name)?;
    
    let mut common_channels = None;
    
    for driver in drivers {
        let space_info = DriverSpaceInfo::build_from_db(&db, &driver.dll_path).await?;
        let actual_space = space_info.space_generator.map_virtual_to_actual(...)?;
        
        let available = db.get_channels_by_space(
            &driver.dll_path,
            actual_space,
        )?;
        
        if let Some(ref mut common) = common_channels {
            *common = intersection(*common, available);
        } else {
            common_channels = Some(available);
        }
    }
    
    Ok(common_channels.unwrap_or_default())
}
```

---

### フェーズ4: データベースクエリの追加

#### 4.1 グループ関連クエリ
**ファイル**: `recisdb-proxy/src/database/bon_driver.rs`

```rust
/// group_name から複数のドライバーを取得
pub fn get_group_drivers(&self, group_name: &str) -> Result<Vec<BonDriverRecord>> { }

/// group_nameを自動推測（DLL名から）
pub fn infer_and_set_group_name(&self, driver_id: i64) -> Result<Option<String>> { }

/// グループ内の有効なドライバーパスを取得
pub fn get_group_driver_paths(&self, group_name: &str) -> Result<Vec<String>> { }
```

#### 4.2 チャンネルの帯域・地域情報が必要
**既存テーブル拡張**: channels テーブ

```sql
-- 帯域分類をキャッシュ（正規化）
ALTER TABLE channels ADD COLUMN band_type INTEGER;  -- BandType enum
ALTER TABLE channels ADD COLUMN terrestrial_region TEXT;  -- "福島", "宮城" など
```

---

### フェーズ5: プロトコル拡張

#### 5.1 ClientMessage拡張
**ファイル**: `recisdb-protocol/src/types.rs`

```rust
pub enum ClientMessage {
    // 既存...
    
    // グループ指定でのOpenTuner
    OpenTunerWithGroup {
        group_name: String,
    },
    
    // グループ指定でのSetChannelSpace（仮想空間インデックス）
    SetChannelSpaceInGroup {
        group_name: String,
        space_idx: u32,      // 仮想空間インデックス
        channel: u32,        // チャンネル
        priority: i32,
        exclusive: bool,
    },
    
    // グループ内のチャンネル列挙
    EnumChannelNameInGroup {
        group_name: String,
        space_idx: u32,
        channel: u32,
    },
}
```

#### 5.2 ServerMessage拡張
```rust
pub enum ServerMessage {
    // 既存...
    
    OpenTunerWithGroupAck {
        success: bool,
        selected_driver: Option<String>,
        error: Option<String>,
    },
    
    EnumChannelNameInGroupAck {
        channels: Vec<String>,
    },
}
```

---

## 実装の優先順位

1. **フェーズ1（基盤）**: BonDriverグループ統一名
   - スキーマ変更
   - グループマッピング

2. **フェーズ2（コア）**: チューナー空間自動生成
   - 帯域分類の強化
   - SpaceGeneratorの実装
   - 地域別の空間割当

3. **フェーズ3（統合）**: グループ内選択ロジック
   - Selectorの拡張
   - Sessionのハンドラ追加

4. **フェーズ4（DB）**: クエリとスキーマ補完

5. **フェーズ5（通信）**: プロトコル拡張（必要に応じて）

---

## チャレンジと対策

### チャレンジ1: 複数DLLでの帯域・地域の一致性
**問題**: MLT1.dll は福島地上波のみ、MLT2.dll は福島+宮城、MLT3.dll は福島+宮城+BS...

**対策**:
- グループ内でも各ドライバーのチャンネル可用性は異なることを許容
- 仮想space_idx から各ドライバーの実bon_spaceへのマッピングは「ベストエフォート」
- ドライバーのサポート可能チャンネルに応じて無効空間を返す

### チャレンジ2: space_idx の解釈の一貫性
**問題**: TVTest がspace_idx を送るが、異なるドライバーで異なる意味になる可能性

**対策**:
- グループ単位で「カノニカルな空間割当」を定義
- グループ内の全ドライバーの **和集合** チャンネルから空間を構築
- 個別ドライバーへのマッピング時に足りない場合は適切にエラー処理

### チャレンジ3: NID からの地域推定
**問題**: NID 0x7FE0 が福島か宮城か不明

**対策**:
- `broadcast_region.rs` の NID マップを拡充
- チャンネルスキャン時に `raw_name` や `channel_name` から地域名を推測
- ユーザーによる手動設定も可能に

---

## 実装フロー

```
1. スキーマ拡張
   ↓
2. Models 更新
   ↓
3. BonDriver group_name の検出・設定
   ↓
4. SpaceGenerator 実装
   ↓
5. DriverSpaceInfo 実装
   ↓
6. Selector 拡張
   ↓
7. Session ハンドラ追加（OpenTuner, SetChannelSpace, EnumChannelName）
   ↓
8. テスト・検証
```

---

## 関連ドキュメント

- [ARCHITECTURE.md](../ARCHITECTURE.md): 全体設計
- [BonDriverIntegratedPlan.md](../BonDriverIntegratedPlan.md): BonDriver統合計画
- [broadcast_region.rs](../../recisdb-protocol/src/broadcast_region.rs): NID分類

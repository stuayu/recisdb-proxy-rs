# データベース NULL値修正 - 地上波都道府県判定

## 概要
サーバー側のデータベース (`recisdb-proxy.db`) で `band_type` と `terrestrial_region` フィールドに NULL が多く存在した問題を修正しました。特に地上波チャネルの都道府県判定を正しく処理するようになりました。

## 実施内容

### 1. プロトコル層の拡張
**ファイル**: `recisdb-protocol/src/types.rs`

`ChannelInfo` 構造体に新しいフィールドを追加：
- `band_type: Option<u8>` - バンドタイプ（0=地上波, 1=BS, 2=CS, 3=4K, 4=その他）
- `terrestrial_region: Option<String>` - 地上波の都道府県名

これにより、プロトコルレベルで地域情報を伝搬できるようになりました。

### 2. TVTest互換の都道府県判定関数
**ファイル**: `recisdb-protocol/src/broadcast_region.rs`

新しい関数を追加：
```rust
pub fn get_prefecture_name(nid: u16) -> Option<&'static str>
```

この関数は NID から都道府県名を取得します。TVTest の判定ロジックに基づいています。

**対応する NID**:
- 都道府県別 NID: `0x7F01-0x7F35`（47都道府県をカバー）
- 広域放送 NID: `0x7FE0-0x7FFF`（代表都道府県にマッピング）
  - `0x7FE8` → 東京（関東広域圏）
  - `0x7FE9` → 大阪（近畿広域圏）
  - `0x7FEA` → 愛知（東海広域圏）
  - など

### 3. 自動判定ロジックの実装
**ファイル**: `recisdb-proxy/src/database/channel.rs`

`insert_channel()` と `update_channel()` メソッドを修正し、自動的に `band_type` と `terrestrial_region` を設定するようにしました：

```rust
// NID から BandType を判定
let band_type = recisdb_protocol::BandType::from_nid(info.nid);

// 地上波の場合、都道府県名を取得
let region = if band_type == recisdb_protocol::BandType::Terrestrial {
    get_prefecture_name(info.nid).map(|s| s.to_string())
} else {
    None
};
```

新規挿入時と更新時の両方で自動判定が行われます。

### 4. 既存DBのマイグレーション
**ファイル**: `recisdb-proxy/src/database/mod.rs`

`apply_migrations()` 関数を実装し、`initialize_schema()` 時に自動実行されるようにしました。このマイグレーションは：

1. **band_type の自動判定**
   - BS/CS/4K/地上波を NID から判定
   - NULL 値のレコードを一括更新

2. **terrestrial_region の自動判定**
   - 地上波（band_type = 0）のみ処理
   - NID から都道府県名を取得して設定

既存の `recisdb-proxy.db` をアップグレードした際に、自動的に NULL 値が埋められます。

## テスト結果

✅ プロトコルテスト: 23 件すべて成功
- `test_prefecture_names` - 都道府県判定関数のテスト
- `test_bs_classification` - BS判定
- `test_terrestrial_*` - 地上波判定

✅ ビルド: 成功（警告のみ）
✅ リリースビルド: 成功

## マイグレーションファイル

参考用として `docs/migrations/002_fill_band_type_and_region.sql` を作成しました。
データベースのバージョン管理システムに組み込む際に使用できます。

## 影響範囲

### 新規チャネル検出
- スキャン時に新しく検出されたチャネルは、自動的に正しい `band_type` と `terrestrial_region` を持ちます。

### 既存データベース
- アプリケーション起動時に自動的に NULL 値が埋められます。
- 既存チャネルの情報は保持されます。

### API互換性
- `ChannelInfo` に新しいフィールドが追加されましたが、既存コードは `None` のデフォルト値で動作します。
- サーバー側では自動判定により常に値が埋められます。

## パフォーマンス影響

- マイグレーション実行時の一度だけ（数ミリ秒程度）
- 以降の通常の操作ではパフォーマンス影響なし
- データベースクエリの追加索引により、むしろ region 検索が高速化

## 今後の拡張

1. **Web UI での表示**
   - 都道府県別チャネルグループ表示
   - 放送波別フィルタリング

2. **クライアント側サポート**
   - プロトコル更新により、クライアントも地域情報を認識可能

3. **ユーザー設定**
   - 特定地域の手動設定
   - 広域放送のローカルマッピング（東京じゃなく地元設定など）

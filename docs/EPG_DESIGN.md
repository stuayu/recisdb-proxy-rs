# EPG(番組表)収集・配信設計

recisdb-proxy の番組表データがどこから来て、どう保存され、どう読まれるかの台帳。
実装は 2026-07 時点のコードに対応する。

## 全体像

```
チューナーリーダースレッド (tuner/shared.rs)
  └─ 生TS (B25デコード前) を毎チャンク EpgCollector に渡す
       EpgCollector (tuner/epg_collector.rs)
         ├─ PID 0x0012 の EIT パケットを SectionCollector で再組み立て
         ├─ EitTable::parse (ts_analyzer/eit.rs) でイベント抽出
         └─ ProgramUpsert をプロセス全域 mpsc チャネルへ送信
             EpgWriter (epg_writer.rs, tokioタスク)
               ├─ (nid, sid, event_id) でメモリ上デデュープ
               ├─ 10秒 or 500件ごとに programs テーブルへ一括UPSERT
               └─ 約5分ごとに終了後24時間経過した行を削除
                   programs テーブル (database/program.rs, Migration 015)
                     ├─ GET /api/programs        (web/api/programs.rs, ダッシュボード)
                     └─ GET /api/programs (Mirakurun互換, web/mirakurun.rs)
```

## 収集(受動収集)

- **専用の EPG クローラーは存在しない。** クライアントが選局してチューナーが動いている間だけ、
  そのTSに流れている EIT を拾う(ロゴ収集 `logo_collector.rs` と同じ「ついで収集」方式)。
- 対象は PID 0x0012 の全 EIT テーブル:
  - **p/f(present/following)**: table_id 0x4E(自TS)/ 0x4F(他TS)。現在番組・次番組。
    リアルタイムの「今なにやってる」情報はここから入る。送出周期が短いので選局直後にすぐ埋まる。
  - **schedule**: 0x50–0x5F(自TS)/ 0x60–0x6F(他TS)。数日先までの番組表。
    BS は他TS向け schedule を各TSが相互に流しているため、BS 1チャンネルの視聴で
    BS 全体の番組表が埋まりやすい。地上波は自TS分が基本。
- p/f と schedule は区別せず同じ `programs` テーブルに UPSERT される。同一イベントは
  `(nid, sid, event_id)` で一意なので、p/f で先に入った行が schedule で上書きされる(逆も同様)。
  常に `updated_at`(収集時刻)が新しい方が勝つ。
- 収集は B25 デコード**前**の生TSに対して行う。EIT は非スクランブルなのでデコード不要。
- `EpgCollector` はリーダータスク起動ごとに作り直されるため、選局切替・再接続で
  PSI 再組み立て状態は自然にリセットされる。

### なぜ mpsc チャネル経由か

リーダーループは `TunerPool` → `SharedTuner` → OSスレッドの深部で回っており、
`Database` ハンドルが配管されていない。`EpgCollector` はパースだけを行い、
`epg_collector::set_global_sender`(`OnceLock`)に登録されたプロセス全域チャネルへ
送るのみ。送信先未登録(単体テスト・recisdb CLI)の場合は黙って捨てる(best-effort)。
詳細は `tuner/epg_collector.rs` のモジュールコメント参照。

### セクション再組み立ての要点(2026-07 修正)

EIT PID はセクションが隙間なく詰まって流れるため、`SectionCollector`
(`ts_analyzer/psi.rs`)は「1パケット=最大1セクション」ではなく
**バイトストリームとして扱い、1回の `add_data` で0〜N個の完成セクションを返す**:

- PUSI パケットの pointer_field 手前のバイトは、組み立て中セクションの最終断片として
  先に消費・完成させる(捨てない)。
- back-to-back の複数セクション、パケット境界をまたぐ3バイトヘッダにも対応。
- table_id 0xFF(スタッフィング)で停止、`section_length > 4093` は破棄。
- **emit 前に CRC32 検証**を行い、壊れたセクションは番組表に入らない。

かつてはこの3点に欠陥があり、視聴中でも EIT セクションの大半を取りこぼして
番組表が歯抜けになっていた(同じ collector を使うロゴ収集・PAT/PMT フィルタも同様)。
リグレッションテストは `psi.rs` の `test_tail_and_head_share_pusi_packet` ほか。

## 保存

- スキーマ: `programs` テーブル(Migration 015、`database/mod.rs` の台帳)。
  UNIQUE `(nid, sid, event_id)`。列はコード(`database/models.rs::ProgramRecord`)を正とする。
- 書き込みは `EpgWriter`(`main.rs` で1度だけ spawn)経由のみ:
  - メモリ上で `(nid, sid, event_id)` デデュープ後、**10秒間隔 or 500件到達**でフラッシュ
    (1トランザクションで一括UPSERT)。
  - UPSERT は `WHERE excluded.updated_at >= programs.updated_at` 付きで、
    古いバッチが新しい行を上書きしない。
  - **prune**: 30フラッシュごと(≒5分)に、終了時刻が現在より24時間以上前の行を削除。
    未来分の保持期限はない(schedule で入った分はそのまま残る)。
- 時刻はすべて epoch 秒(UTC)。EIT の MJD+BCD(JST壁時計)は `eit.rs` で変換済み。

## 読み出し

| API | 用途 |
|---|---|
| `GET /api/programs?since=&until=&nid=&sid=` | Webダッシュボードの番組表タブ。`[start_at, start_at+duration)` が `[since, until)` と重なる行を返す |
| Mirakurun互換 `GET /api/programs` | EPGStation 等の録画クライアント向け |

## 既知の制約

- **受動収集ゆえ、視聴していないネットワークの番組表は増えない。** 特に地上波は
  そのチャンネル(物理TS)を選局しないと埋まらない。BS/CS は1チャンネル視聴で広く埋まる。
- **schedule EIT の送出周期は遠い日付ほど長い**(地上波で数分オーダー)。選局直後は
  直近数時間ぶんから埋まり、先の日付が揃うには数分〜十数分の視聴継続が必要。
- 誰も視聴していない時間帯は更新が完全に止まる。長時間止まると p/f 由来の
  「現在・次」情報は古くなる(prune は終了済み番組しか消さないため、歯抜けではなく
  stale として現れる)。定期巡回収集(EPGクローラー)は未実装・将来課題。

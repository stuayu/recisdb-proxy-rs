# EPG(番組表)収集・配信設計

recisdb-proxy の番組表データがどこから来て、どう保存され、どう読まれるかの台帳。
実装は 2026-07 時点のコードに対応する。

## 全体像

```
チューナーリーダースレッド (tuner/shared.rs)
  └─ 生TS (B25デコード前) を毎チャンク EpgCollector に渡す
       EpgCollector (tuner/epg_collector.rs)
         ├─ PID 0x0012/0x0026/0x0027 の EIT パケットを SectionCollector で再組み立て
         ├─ EitTable::parse (ts_analyzer/eit.rs) でイベント抽出
         └─ ProgramUpsert をプロセス全域 mpsc チャネルへ送信
             EpgWriter (epg_writer.rs, tokioタスク)
               ├─ (nid, sid, tsid, event_id) でメモリ上デデュープ
               ├─ 10秒 or 500件ごとに programs テーブルへ一括UPSERT
               └─ 約5分ごとに終了後24時間経過した行を削除
                   programs テーブル (database/program.rs, Migration 015/025/026)
                     ├─ GET /api/programs        (web/api/programs.rs, ダッシュボード)
                     └─ GET /api/programs (Mirakurun互換, web/mirakurun.rs)
```

## 収集(受動収集)

- **専用の EPG クローラーは存在しない。** クライアントが選局してチューナーが動いている間だけ、
  そのTSに流れている EIT を拾う(ロゴ収集 `logo_collector.rs` と同じ「ついで収集」方式)。
- 対象は PID 0x0012 (H-EIT) と、TR-B14 の M-EIT/L-EIT PID 0x0026/0x0027:
  - **p/f(present/following)**: table_id 0x4E(自TS)/ 0x4F(他TS)。現在番組・次番組。
    リアルタイムの「今なにやってる」情報はここから入る。送出周期が短いので選局直後にすぐ埋まる。
  - **schedule**: 0x50–0x5F(自TS)/ 0x60–0x6F(他TS)。数日先までの番組表。
    BS は他TS向け schedule を各TSが相互に流しているため、BS 1チャンネルの視聴で
    BS 全体の番組表が埋まりやすい。地上波は自TS分が基本。
- p/f と schedule は区別せず同じ `programs` テーブルに UPSERT される。同一イベントは
  `(nid, sid, tsid, event_id)` で一意なので、p/f で先に入った行が schedule で上書きされる(逆も同様)。
  常に `updated_at`(収集時刻)が新しい方が勝つ。
- 収集は B25 デコード**前**の生TSに対して行う。EIT は非スクランブルなのでデコード不要。
- `EpgCollector` はリーダータスク起動ごとに作り直されるため、選局切替・再接続で
  PSI 再組み立て状態は自然にリセットされる。

#### 地上波 EIT の運用規定 (ARIB TR-B14)

TR-B14 Version 6.7 Vol. 4 表13-7 (PDF printed p. 83) は PID を
`0x0012=H-EIT`、`0x0026=M-EIT`、`0x0027=L-EIT` と定義する。これは地上/衛星の
区別ではなく、固定受信機/移動受信機/部分受信機向け EPG 種別の区別である
(§13.1、PDF printed pp. 64-66)。従って `EIT_TERRESTRIAL`/`EIT_SATELLITE` という
名称は誤りで、コードでは `EIT_MOBILE`/`EIT_PARTIAL_RECEPTION` と呼ぶ。

地上波は other TS の EIT を送出せず、actual TS のみ (§13.1、PDF printed p. 64)。
H-EIT の table_id は p/f `0x4E`、schedule basic `0x50-0x57`、schedule extended
`0x58-0x5F`、M-EIT/L-EIT は `0x4E` (表13-8、PDF printed p. 83)。共通パーサは
衛星の other TS EIT も扱うため全範囲を構文上受け入れるが、地上波判定を持つ呼出し側は
`is_terrestrial_eit_table_id` で `0x4F`/`0x60-0x6F` を除外する。

H-EIT[schedule] は1 segment=3時間、1 segment 最大8 section。イベントがない segment
にも空 section を最低1つ送出し、過去 segment は送出停止する (TR-B14 §13.11.3、
PDF printed pp. 85-86)。送出周期は固定値でなく BIT の SI Parameter Descriptor の
repetition-rate group に従い、既定値は H-EIT[p/f]=1秒、schedule basic の基本=60秒、
拡張 group 1=3秒、拡張 group 2=10秒 (表12-6、PDF printed p. 58)。

sub-table 更新時は同一 sub-table に新旧 version を混在させず、H-EIT[schedule]・
M-EIT・L-EITにも適用する (§12.8、PDF printed p. 62)。実装は PIDを含む
`(pid, table_id, service_id, transport_stream_id, original_network_id)` ごとに5-bit
modulo versionを管理し、`current_next_indicator=0` を破棄する。取得完了は section の
受信だけで判断せず、segment の空 sectionを含む運用と再送周期のため、未受信イベントを
削除する完了処理は実装しない。

#### BS4K/高度BS (MMT/TLV)

4Kの生入力は MPEG-2 TS ではない。`mmt_pipe.rs` が dantto4k を通し、変換後の
TSだけをこの collector へ渡す。dantto4k の実装 (`remuxerHandler.cpp::onMhEit`)
は、MMTP Packet ID `0x8000` の MH-EIT を TS PID `0x0012` へ変換する。
MH-EIT の table_id は p/f `0x8B`→TS `0x4E`、schedule `0x8C..0x9B`→TS
`0x50..0x5F`。したがって4Kについて `0x0026/0x0027` を読む必要はなく、既存の
`0x0012` 経路で拾える。

根拠は ARIB STD-B60 §4.3 表4-11/4-12 (PDF pp.22-23: MH-EITはMMTP
Packet ID `0x8000`)、§7.3.3.9 表7-18 (PDF pp.66-68: MH-EIT構造)、ARIB
TR-B39 Part 1 Vol.4 §5.1/5.2 表5.1-3/5.2-4 (PDF pp.37-40: message/table
IDと `0x8B`, `0x8C..0x9B`)、§14.1 (PDF pp.83-87: p/f、schedule、3時間segment、
最大8 section/segment) による。

MH-EITの `tlv_stream_id` はTSの `transport_stream_id` そのものではないが、
dantto4kは同値をTSID欄へ写す。`original_network_id`、`service_id`、`event_id`も
対応欄へ保持するため、現在の `(nid, sid, tsid, event_id)` 主キーと整合する。
TR-B39 Part 1 Vol.3 §5.5 表5.5-1 (PDF pp.43-44) は、`tlv_stream_id`をネットワーク内で
一意、`service_id`をサービスチャンネル、`event_id`をサービス内で一意と定義する。

MH-EITの `start_time` は40-bit MJD+BCDのJST、`duration`は24-bit BCDであり、
STD-B60 §7.3.3.9 (PDF pp.67-68) の値をdantto4kがTS EITへ写す。未定値はall-1。
本実装は開始時刻未定を捨て、duration未定を0秒保存する。8K/22.2ch、MH固有の
音声・映像属性はTS EITに写像されても `programs` には保存しない。これは今回の
主キー・時刻・基本番組名収集の不具合ではなく、メタデータ拡張の未実装。

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
- TS入力チャンクが188バイト境界で分割されても、`EpgCollector` が未完了バイトを次回へ保持する。
- `current_next_indicator=0` は次回版なので破棄し、現行sub-tableのversion_numberを5-bit moduloで管理する。

かつてはこの3点に欠陥があり、視聴中でも EIT セクションの大半を取りこぼして
番組表が歯抜けになっていた(同じ collector を使うロゴ収集・PAT/PMT フィルタも同様)。
リグレッションテストは `psi.rs` の `test_tail_and_head_share_pusi_packet` ほか。

## 保存

- スキーマ: `programs` テーブル(Migration 015/025/026、`database/mod.rs` の台帳)。
  UNIQUE `(nid, sid, tsid, event_id)`。service_id/event_id はTS内の識別なのでTSIDを含める。
  `free_ca_mode` はEITのfree_CA_modeを保持し、Mirakurunの`isFree`へ反映する。
- 書き込みは `EpgWriter`(`main.rs` で1度だけ spawn)経由のみ:
  - メモリ上で `(nid, sid, tsid, event_id)` デデュープ後、**10秒間隔 or 500件到達**でフラッシュ
    (1トランザクションで一括UPSERT)。
  - UPSERT は `WHERE excluded.updated_at >= programs.updated_at` 付きで、
    古いバッチが新しい行を上書きしない。
  - **prune**: 30フラッシュごと(≒5分)に、終了時刻が現在より24時間以上前の行を削除。
    未来分の保持期限はない(schedule で入った分はそのまま残る)。
- 時刻はすべて epoch 秒(UTC)。EIT の MJD+BCD(JST壁時計)は `eit.rs` で変換済み。
  start_time all-1 はイベントを除外、duration all-1 は終了時刻不明として0秒で保存する。

## 読み出し

| API | 用途 |
|---|---|
| `GET /api/programs?since=&until=&nid=&sid=` | Webダッシュボードの番組表タブ。`[start_at, start_at+duration)` が `[since, until)` と重なる行を返す |
| Mirakurun互換 `GET /api/programs` | EPGStation 等の録画クライアント向け |

## 既知の制約

### 自動取得設定のDB管理

自動取得のRuntime設定は `epg_global_settings`、`epg_scan_presets`、
`physical_tuner_epg_settings` を正とする。設定ファイルには置かない。Migration 027が
singleton global、system preset、scan state/history/retentionの初期行を冪等に作る。Migration
028はglobalのpreset選択、029はcoverage集計用index、030はスキャン状態を
(network_id, tsid)単位へ移行する。旧singletonの状態行だけは破棄し、番組・設定データは保持する。
`database/epg_settings.rs` のresolverが global → preset → physical tuner override の順で
effective値を作り、API/UIはeffective値とsourceを表示する。現在の永続チューナーidentityは
既存 `bon_drivers.id`（`/api/tuners/:id`）であり、tuner instance用の新identityは追加しない。

設定更新はDBへ直ちに保存され、schedulerは次回評価で再読込する。Active scanは既存readerの
subscriptionからEITを収集し、EpgWriterへ渡す。最小/最大dwell、idle timeout、CPU limit、
同時数を適用し、開始/完了/失敗をstate/historyへ記録する。EpgWriterのflush後とscheduler
判定前にprogramsからcoverage_until/last_eit_received_atを再集計する。remote node側metadata
実行は認証済み `POST /node/v3/epg/metadata` を使う。remote側は通常の `acquire()` と
broadcast購読側の `EpgCollector::new_metadata()` で解析し、番組情報だけ返す。schedulerと
`LocalMuxServer` は別系統の `MuxLeaseManager` を共有し、TTLまたはguard dropで同一
(NID,TSID)の重複を防ぐ。

経路の選択は純関数 `choose_execution_path()` が行い、`allow_remote` / `prefer_local` /
`remote_prefer_metadata_execution` / `remote_allow_ts_transport` の4設定だけで決まる。
`allow_remote=false` なら remote を選ばない。remote を選んだ場合、
`remote_prefer_metadata_execution=true` なら TS を引かずに metadata RPC を使い、
`remote_allow_ts_transport=false` のときは TS 転送経路を選ばない。remote 実行が失敗した
ときは `RemoteMetadataFailed` を記録してローカル収集へフォールバックする。
`remote_prefer_metadata_execution=false` かつ `remote_allow_ts_transport=true` の場合は、
既存の `RemoteMuxStream` を broadcast 購読し、こちら側の `EpgCollector` で解析する。
remote TS 収集も metadata 収集も同じ dwell・lease・失敗時local fallbackを使う。

スキャン状態は各物理TSを表す `(network_id, tsid)` ごとに保持する。EpgWriterのflush後および
スケジューラ評価前に `programs` を同じキーでGROUP BYし、各系統の最終番組終了時刻を
`coverage_until`へ反映する。状態APIの全体coverageは系統ごとの最小値であり、1系統だけ
埋まった状態を全体正常とは扱わない。スケジューラは全BonDriverの有効チャンネルを重複排除し、
coverage不足・stale・failure/backoffを使って次のTSを選ぶ。対象選択は純粋関数で、固定の
先頭チャンネルには依存しない。

BS/CSではEIT parserが返す `original_network_id` と `transport_stream_id` をcollectorが
そのまま `ProgramUpsert.nid/tsid` に設定する。Other-TS EITで供給された系統も同じcoverage
集計へ入るため、目標coverageを満たす系統は直接選局しない。4KはNID分類を維持するが、
MMT/TLVからdantto4kが変換したTSだけを既存collectorへ渡すため、EPG scheduler側で独自に
B25やMMT処理は行わない。

延期・失敗理由は `EpgReasonCode` enumから生成する `{code, details}`形式で保存・配信する。
CPU値、対象TS、次回時刻などの付加情報はdetailsに入れ、画面側はコード対応表だけを持つ。
スケジューラの延期判定は毎回 `epg_scan_states.last_failure_reason` を更新するが、
`epg_scan_history` への deferred 行は同じ理由が継続する間は追加しない(edge-triggered)。
`not_due` は正常な待機状態なのでstateだけに残し、履歴を増やさない。CPU soft/hard上限、
チューナー不足、backoff、無効化、互換チューナーなし、取得失敗はそれぞれ対応する
reason codeと対象TS・CPU値・次回時刻などを記録する。複数条件はdetailsの
`additional_codes`で同時に返す。

- **受動収集ゆえ、視聴していないネットワークの番組表は増えない。** 特に地上波は
  そのチャンネル(物理TS)を選局しないと埋まらない。BS/CS は1チャンネル視聴で広く埋まる。
- **schedule EIT の送出周期は遠い日付ほど長い**(地上波で数分オーダー)。選局直後は
  直近数時間ぶんから埋まり、先の日付が揃うには数分〜十数分の視聴継続が必要。
- 誰も視聴していない時間帯は更新が完全に止まる。長時間止まると p/f 由来の
  「現在・次」情報は古くなる(prune は終了済み番組しか消さないため、歯抜けではなく
  stale として現れる)。active scan schedulerは受動収集経路を選局して補完する。

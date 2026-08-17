# TS 配信・信頼性・プレビュー 設計書

対象: recisdb-proxy の TS データパス全体
目的: リアルタイム性と信頼性(パケットロス抑止)の両立、固定秒数バッファによる安定化、
tsreplace 複数ストリームの高速化、ブラウザプレビュー、およびエコシステム連携。

正本: [DESIGN.md](DESIGN.md) / 課題台帳: [REVIEW_2026-07.md](REVIEW_2026-07.md)
最終更新: 2026-07-02

---

## 0. 要約 (先に結論)

1. **「パケロス NG」と「低遅延」は本質的に両立しない**ので、ストリームを 3 クラス
   (視聴 / 録画 / プレビュー) に分け、クラス毎にバッファ方針とドロップ可否を変える。
   録画クラスは「無損失・大バッファ・満杯なら遅延ではなくエラー化」、視聴クラスは現行の低遅延・最古破棄を維持。
2. **固定秒数バッファ**は「ビットレート × 秒数」でサイズを動的決定するプリフィル+ジッタバッファとして実装。
   配信開始時に N 秒ぶん貯めてから流し始め、以降はその余裕でジッタとザッピングを吸収する。
3. **損失は必ず検出・計測してから語る**。TS の continuity_counter を PID 毎に監視し、
   「どこで何パケット落ちたか」を per-PID で可視化する。今は broadcast の Lagged 件数しか見ていない。
4. **tsreplace が複数ストリームで遅いのは、セッション毎にエンコーダを起動して HW エンコード枠を奪い合うから**。
   (a) 同一チャンネル+プロファイルのエンコード結果を共有、(b) 同時エンコード数をセマフォで制限、
   (c) セッションからエンコードを分離してプール化、の 3 段で解く。
5. **ブラウザプレビュー**は「tsreplace で H.264/AAC 低ビットレートに変換 → HTTP チャンク配信 → mpegts.js」。
   地上波は MPEG-2 Video なので**変換なしでは主要ブラウザで再生できない**点が設計上の制約。これは上記 4 の
   共有エンコーダ基盤にそのまま乗る。
6. ついでに **HTTP-TS ストリーミングエンドポイント**と **Mirakurun 互換 API** を足すと、
   VLC / ffmpeg / EPGStation / KonomiTV / web からそのまま使えるようになる。

---

## 1. 現状の TS データパス (実装確認済み)

```
BonDriver DLL / char device
   │  spawn_blocking 内の読み取りループ (SharedTuner)
   │  256KB チャンク (TS_CHUNK_SIZE = 262144)
   │  読む → B25 デコード → 配る だけ (SI 収集はここではやらない)
   ▼
broadcast::channel(4096)               ← 全購読セッションへファンアウト (BROADCAST_CAPACITY)
   │  遅い購読者は RecvError::Lagged(n) → packets_dropped += n、carry クリア
   ├─→ SI 収集タスク (spawn_si_collector): SDT/CDT → ロゴ、EIT → EPG
   │     subscribe_untracked (購読数に計上しない) + Weak
   ▼
Session (単一 async タスク)
   │  ts_send_carry で 188 バイト境界に再アライメント (0x47 同期)
   │  ただし「残りなし・188 の倍数・先頭が同期バイト」なら carry を迂回して素通し
   │  [任意] tsreplace パイプ (SID 毎に連鎖、OS パイプ)
   ▼
mpsc::channel(256)  TS 用 (TS_WRITE_BUFFER_CAPACITY)
   │  try_send → Full なら最古を捨てる (drop-oldest)
   ▼
writer タスク (専用) → TCP ソケット
   │  BNDP フレーム [Magic|Len|Type|Payload]
   ▼
クライアント DLL
   │  TsRingBuffer  RING_BUFFER_SIZE = 188 × 1024 × 100 ≈ 19.2MB (Condvar 通知)
   │  満杯時は最古を上書き
   ▼
TVTest
```

### 現状のドロップ点 (= パケロス発生源) と問題

| # | 場所 | 挙動 | 問題 |
|---|------|------|------|
| D1 | broadcast (server) | 遅い購読者は Lagged で**まとめて欠落**、carry クリア | 欠落を「件数」でしか把握できず、どの PID/どの番組が壊れたか不明。録画中でも黙って落ちる |
| D2 | TS 書き込み mpsc(256) | 満杯で最古破棄 | ネットワーク輻輳時に無警告でロス。録画では致命的 |
| D3 | クライアント TsRingBuffer(19MB) | 満杯で最古上書き | TVTest 側の取り出しが遅いと無警告でロス |
| D4 | tsreplace 入力キュー Full | stall 扱い → passthrough or 切断 | エンコーダが詰まると映像が飛ぶ or 切れる |

**現状はすべて「低遅延優先の最古破棄」**。视聴には妥当だが「パケロス NG」の要件には応えていない。
また **損失検出は D1 の件数のみ**で、CC ベースの正確な損失計測は配信経路に接続されていない
(`ts_analyzer/packet.rs` に continuity_counter パースはあるが品質集計は別系統)。

---

## 2. 信頼性モデル: ストリームクラス

配信を 3 クラスに分類し、セッション確立時 (または Mirakurun 互換 API の priority) で決定する。

| クラス | 代表 | 遅延 | ロス許容 | バッファ方針 | 満杯時の挙動 |
|--------|------|------|----------|--------------|--------------|
| **VIEW** (視聴) | TVTest 視聴 | 最小 | 可 (最古破棄) | 小 (0.5–1.5s) | drop-oldest (現行踏襲) |
| **RECORD** (録画) | 録画クライアント / EPGStation | 大きくてよい | **不可** | 大 (5–15s、ビットレート連動) | **落とさず遅延吸収 → それでも溢れたらエラーとして切断・記録** |
| **PREVIEW** (プレビュー) | ブラウザ mpegts.js | 中 | 可 | 中 (2–4s) | drop-oldest + キーフレーム待ち再同期 |

クラスは以下で判定:
- BNDP: `Hello` 後の優先度、または新設 `StreamClass` フィールド (プロトコル v2)。
- HTTP: エンドポイントとクエリ (`?mode=record` 等)。
- 既定は VIEW。priority ≥ 録画しきい値なら RECORD に自動昇格。

**設計判断**: 「パケロス NG」を全クラスに一律適用しない。録画だけを無損失にすることで、
視聴のザッピング応答性(Round 3 で作り込んだ即 ACK 設計)を壊さない。

---

## 3. パケットロス対策

### 3.1 検出 — まず「落ちた事実」を正確に掴む

1. **per-PID continuity_counter 監視**を配信経路に接続する。
   各 PID の CC が `+1 (mod 16)` でなければ欠落(adaptation の discontinuity_indicator は除外)。
   PID 毎の欠落数・直近欠落時刻を `SessionRegistry` に集計し、Web/metrics に出す。
2. **transport_error_indicator (bit)** と **transport_scrambling_control** も集計 (既存の scrambled/error 系を CC 集計と統合)。
3. **broadcast Lagged (D1)** はチャンク単位でしか分からないので、落ちた区間を「不明欠落」として別カウント。
4. 端点別に集計: 「チューナー入力」「broadcast」「エンコーダ」「ネットワーク送出」のどこで発生したかをラベル付け。

これにより「配信は正常だが受信 PC が遅い」等の切り分けがダッシュボードで可能になる。

### 3.2 回避 — クラス別のバックプレッシャ

現状の単一 `broadcast(4096)` は「1 つの遅い購読者のために全員の設定が固定」で、
かつ購読者毎の適切なバッファ制御ができない。次の構造にする。

```
SharedTuner ──chunk──▶ Fanout
                         ├─ VIEW subscriber   : bounded queue, drop-oldest (小)
                         ├─ PREVIEW subscriber : bounded queue, drop-oldest + 再同期 (中)
                         └─ RECORD subscriber  : 大容量 queue、drop 禁止
                                                  溢れそうなら:
                                                   1) writer 側を優先スケジュール
                                                   2) それでも溢れたら QueueOverflow エラーで切断
                                                      (session_history に理由記録)
```

- broadcast は維持しつつ、**RECORD クラスは broadcast の受信を専用タスク+大容量中間キュー**で受け、
  Lagged を発生させない (broadcast の per-receiver バッファを録画用途では十分大きく取る、
  または録画のみ mpsc の専用サブスクライブ経路にする)。
- RECORD で本当に送出が追いつかない場合は**黙って落とさずエラーにする**。
  「無損失を約束できないなら、約束を破る前に知らせる」。
- TS 書き込みキュー (D2) は **フレーム数ではなくバイト予算**で制限する (実装済み: `server/ts_queue.rs`)。

#### TS 書き込みキューのバイト予算 (実装済み)

フレーム数での制限をやめた理由: 1 フレーム = broadcast 1 チャンク = リーダーが 1 回で
ドライバから読めた量であり、ローカルチューナー直結なら最大 256KB、上流が別の
recisdb-proxy(多段構成)なら通常 64KB になる。同じ「256 スロット」が構成によって
8 秒にも 32 秒にもなり、拠点間リンクの設計値として使えない。

```
budget_bytes = bitrate_bps / 8 × ts_queue_ms / 1000
```

- `bitrate_bps`: セッションで**実測**した値 (1 秒毎サンプルの EWMA、α=0.3)。
  未計測の間は `prefill::default_bitrate_bps` の帯域別既定値。
  ServiceFilter=single やエンコード配信では実レートが既定値を大きく下回るため、
  実測に寄せないと数倍過大なバッファを確保してしまう。
- `ts_queue_ms`: クラス別。`tuner_config` の
  `ts_queue_view_ms` (既定 8000) / `ts_queue_preview_ms` (12000) / `ts_queue_record_ms` (15000)。
  `GET/PATCH /api/config/tuner` で変更可能 (Web ダッシュボードのフォームには未掲載)。
- 予算はランタイムで変更される: 選局・クラス確定時と、実測ビットレートが 25% 以上動いたとき。
- 空キューには予算超過のフレームも 1 つだけ通す。通さないと 1 チャンクが
  設定窓より大きい構成でストリームが永久に止まる。
- mpsc のスロット数 (8192) は輸送層の保険であり、実際の「満杯」判定はバイト予算が行う。

拠点間中継 (例: 宮城・福島 ↔ 東京) では、**バッファはジッタしか吸収できない**点に注意する。
実効帯域がストリームのビットレートを下回る場合は、いくら秒数を伸ばしても遅延が単調増加して
最後に切れるだけなので、`ServiceFilter=single` かエンコード配信でレート自体を下げること。

### 3.3 再同期 (VIEW/PREVIEW)

欠落後は PES/映像フレーム境界がずれるので、**落とした後は次の PAT/PMT または映像キーフレーム(RAP)まで待って**
から再開すると、プレイヤーのデコードエラーが最小化する (特に PREVIEW/mpegts.js)。
現状は carry クリアのみなので、RAP 検出による再同期を PREVIEW に追加する。

---

## 4. 固定秒数バッファ (プリフィル + ジッタバッファ)

### 4.1 なぜ必要か

チューナー出力はほぼ一定レートだが、ネットワーク・エンコーダ・クライアント取り出しにジッタがある。
配信開始直後は特にバッファが空で、少しの遅れが即ロス/途切れになる。
**開始時に固定秒数ぶん貯めてから流す**ことで、以降のジッタを吸収する余裕を作る。

### 4.2 サイズ決定

```
buffer_bytes = target_bitrate_bps / 8 × prefill_seconds × safety_factor
```

- `target_bitrate_bps`: チャンネルの実測ビットレート。**実装済み**: セッションが 1 秒毎に
  観測した値を EWMA (α=0.3) で保持し、プリフィルと TS 書き込み予算の両方をこれで換算する。
  未計測の間だけ帯域別デフォルト (地上波 ~18Mbps / BS ~24Mbps / 4K ~33Mbps) を使う。
- `prefill_seconds`: クラス別 (VIEW 1s / PREVIEW 2s / RECORD 5–10s)。DB `tuner_config` で調整可能に。
- `safety_factor`: 1.5 程度。

これで「固定秒数」を byte 数に翻訳し、リングバッファ容量を動的に設定する。
今のクライアント固定 19.2MB (≈ 地上波で 8–10 秒) を、**サーバー側で秒数指定 → 実バイトに変換**する形へ一般化する。

### 4.3 プリフィル手順

1. 選局・エンコーダ準備完了後、送出を止めたまま `buffer_bytes` まで蓄積。
2. 満ちたら送出開始。以降は通常のリングとして運用。
3. プリフィル中はクライアントへ「バッファリング中」を通知 (プレビュー UI のスピナー、TVTest は待機)。
4. アンダーラン(枯渇)検知時は再プリフィルするか、VIEW なら即流し続ける (クラス依存)。

### 4.4 設定

`tuner_config` に追加 (migration 011 / 018、いずれも実装済み):
```
prefill_view_ms       INTEGER DEFAULT 1000
prefill_preview_ms    INTEGER DEFAULT 2000
prefill_record_ms     INTEGER DEFAULT 6000
jitter_safety_factor  REAL    DEFAULT 1.5
ts_queue_view_ms      INTEGER DEFAULT 8000     -- §3.2 TS 書き込みキュー
ts_queue_preview_ms   INTEGER DEFAULT 12000
ts_queue_record_ms    INTEGER DEFAULT 15000
```

---

## 5. tsreplace 複数ストリームの高速化

### 5.1 現状と根本原因

- セッション毎に tsreplace を起動 (session.rs)。複数 SID は OS パイプで連鎖 (これ自体は良い設計)。
- **問題**: N セッション × それぞれの tsreplace/QSVEncC が**同時に HW エンコードを要求**する。
  Intel QSV 等の同時エンコードセッション数には上限があり、超えると各々が遅くなる/枯渇する。
- 同じチャンネル・同じ SID を複数人が tsreplace 付きで見ても、**エンコードが重複**して走る。

### 5.2 対策 (3 段、上から順に効果大)

#### (A) 共有エンコード (Shared Encoder) — 最重要
SharedTuner が「生 TS」を共有するのと同様に、**エンコード済み出力も共有**する。

```
EncodeKey = (channel_key, encode_profile)     // プロファイル = 解像度/コーデック/ビットレート等
```

- `EncodePool: HashMap<EncodeKey, Arc<SharedEncoder>>` を新設。
- `SharedEncoder` は 1 本の tsreplace チェーンを持ち、出力を broadcast で複数セッションへ配る
  (SharedTuner と完全に同じパターン)。
- 同一チャンネル+同一プロファイルの視聴/プレビューが束ねられ、**エンコードは 1 回だけ**。
- 参照カウント 0 + keep-alive で終了 (SharedTuner の idle-close を踏襲)。

これで「同じ番組をブラウザ 3 人が見る」ケースがエンコード 1 本になる。

#### (B) 同時エンコード数の上限 (Semaphore)
`tsreplace_config.max_concurrent_encoders`(既定 = HW の同時セッション数目安) を追加し、
グローバル `tokio::sync::Semaphore` で新規エンコーダ起動を絞る。枠が無い場合のポリシー:
- PREVIEW/VIEW: passthrough (生 TS) にフォールバック、または待機。
- RECORD: 優先確保 (低優先エンコーダを止めて枠を譲る、優先度モデルと同じ発想)。

#### (C) セッションからのエンコード分離
現状 tsreplace はセッションのライフサイクルに紐づく。(A) の SharedEncoder に移すことで、
セッション切断でエンコーダが落ちて再起動…を防ぎ、プロセス起動オーバーヘッド(QSV 初期化は重い)を償却する。

### 5.3 プロファイル

`tsreplace_config` を単一設定から**プロファイル表**に拡張:
```
encode_profiles(
  id, name, codec, container, target_bitrate, extra_args,
  purpose  -- 'record' | 'preview' | 'view'
)
```
- 録画は高画質 HEVC、プレビューは低ビットレート H.264 と使い分け。
- プレビュー用プロファイルは §6 のブラウザ再生要件 (H.264/AAC) に合わせる。

### 5.4 セキュリティ注記
コマンドパス/引数を無認証 Web API から変更できる問題 (REVIEW S1) は本設計と独立に P0 で封じる。
プロファイル表化でも `command_path` は TOML 側固定・API からは引数/プロファイル選択のみ、を維持する。

---

## 6. ブラウザプレビュー (Mirakurun 風)

### 6.1 調査結果と前提

- stuayu/Mirakurun フォーク**本体**には mpegts.js 等のプレイヤーは同梱されていない
  (依存に無し。UI は状態/サービス/チューナー管理中心の React/FluentUI)。
  ブラウザ視聴は **ts-live (WASM/WebCodecs/WebGPU)** や **mpegts.js 系クライアント (EPGStation/KonomiTV)** など
  外部クライアントが担うのが実態。
- したがって recisdb-proxy 側で「ブラウザプレビュー」を実現するには、
  **プレイヤーが食える形式で HTTP ストリームを出す**のが本質。自前でプレイヤーも軽く同梱する。

### 6.2 再生方式の選択

| 方式 | 対応映像 | 変換要否 | ブラウザ | 採否 |
|------|----------|----------|----------|------|
| mpegts.js に生 TS | H.264/AAC のみ | 不要 | 主要ブラウザ | 地上波は **MPEG-2 のため不可** |
| **mpegts.js に変換 TS (H.264/AAC)** | 全て | tsreplace で H.264 化 | 主要ブラウザ (MSE) | **採用 (第一選択)** |
| ts-live 相当 (WASM ffmpeg) | MPEG-2 含む全て | 不要 | Chrome (WebGPU) | 高負荷・Chrome 限定。将来オプション |
| HLS/fMP4 | 変換次第 | remux/transcode | iOS Safari 含む | 将来 (録画済み/多デバイス向け) |

**結論**: プレビューは「§5 の共有エンコーダで H.264/AAC 低ビットレートに変換 → HTTP チャンク配信 → mpegts.js」。
MPEG-2 を変換せずブラウザで出す唯一の道は WASM デコード(ts-live 方式)で、これは負荷と対応ブラウザの制約から
将来オプション扱い。

### 6.3 エンドポイント設計 (新設 HTTP ストリーミング)

axum に TS ストリーミングを追加 (現状 web は JSON API のみ)。

```
GET /api/stream/service/:sid            生 TS (H.264/AAC のみのサービス or passthrough)
GET /api/stream/service/:sid?profile=preview   H.264 変換 TS (mpegts.js 用) ← プレビュー
GET /api/stream/channel/:type/:ch/service/:sid  Mirakurun 互換パス (§7)
```

- `Transfer-Encoding: chunked`、`Content-Type: video/mp2t`。
- 内部的には既存の Session/TunerPool/SharedEncoder を HTTP 用の薄いアダプタから呼ぶ
  (BNDP セッションと���じ選局・共有・優先度ロジックを再利用。二重実装しない)。
- クラスは PREVIEW 既定。切断で参照カウント減。

### 6.4 プレビュー UI

ダッシュボードに最小プレイヤーを追加:
- `mpegts.js` を**同梱バンドル** (CSP 制約下でも動くようローカル配信、CDN 依存にしない)。
- サービス一覧から「プレビュー」ボタン → `<video>` + mpegts.js が
  `/api/stream/service/:sid?profile=preview` を再生。
- 字幕 (ARIB-B24) は将来 aribb24.js でオプション対応。
- ライブなので `liveBufferLatencyChasing` 等で遅延を抑制、§4 のプリフィルと協調。

### 6.5 認証
ストリームエンドポイントも Web API と同じトークン認証下に置く (REVIEW S2)。
無認証で映像を垂れ流さない。

---

## 7. エコシステム連携 (有用な追加)

### 7.1 Mirakurun 互換 API (サブセット)
チャンネル DB・サービスフィルタ・TS 配信基盤が既にあるので、費用対効果が最大。
```
GET /api/services                     サービス一覧 (channels テーブルから生成)
GET /api/channels                     チャンネル一覧
GET /api/services/:id/stream          サービスストリーム (TS)
GET /api/channels/:type/:ch/stream    チャンネルストリーム
GET /api/version /api/status          バージョン・状態
```
- これで EPGStation / KonomiTV / mirakc 系クライアントからそのまま利用可能。
- EPG (`/api/programs`) は録画予約に必須だが実装コスト大。まずは視聴系のみのサブセットで良い。

### 7.2 汎用 HTTP-TS 出力
§6.3 のエンドポイントは VLC/ffmpeg からも `http://host/api/stream/...` で直接開ける。
デバッグと相互運用が一気に楽になる。

### 7.3 その他
- **WebSocket/SSE でダッシュボード更新** (現状 5 秒ポーリング): ビットレート・信号・per-PID ロスをリアルタイム表示。
- **録画同時実行の可視化**: 共有エンコーダ・チューナー枠の使用状況をダッシュボードに。

---

## 8. 可観測性 (デバッグ可能性)

- **per-PID / per-端点のロスカウンタ**を Prometheus + ダッシュボードに (§3.1)。
- **バッファ占有率**(プリフィル量・現在の秒数換算)をセッション毎に出す → アンダーラン予兆が見える。
- **エンコーダ稼働数 / 枠使用率 / エンコード遅延**を出す → tsreplace 詰まりが数値で分かる。
- 既存の `session_history` に per-PID ロス要約と disconnect_reason(QueueOverflow 等)を追加。

---

## 9. データモデル追加まとめ

```sql
-- tuner_config へ追加
ALTER TABLE tuner_config ADD COLUMN prefill_view_ms      INTEGER DEFAULT 1000;
ALTER TABLE tuner_config ADD COLUMN prefill_preview_ms   INTEGER DEFAULT 2000;
ALTER TABLE tuner_config ADD COLUMN prefill_record_ms    INTEGER DEFAULT 6000;
ALTER TABLE tuner_config ADD COLUMN jitter_safety_factor REAL    DEFAULT 1.5;

-- tsreplace_config へ追加
ALTER TABLE tsreplace_config ADD COLUMN max_concurrent_encoders INTEGER DEFAULT 2;

-- 新規: エンコードプロファイル
CREATE TABLE encode_profiles(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  purpose TEXT NOT NULL,          -- 'record' | 'preview' | 'view'
  codec TEXT NOT NULL,            -- 'h264' | 'hevc'
  target_bitrate INTEGER,
  extra_args TEXT,
  is_enabled INTEGER DEFAULT 1
);
```

## 10. プロトコル追加まとめ (BNDP v2)

- `Hello` に `stream_class`(u8: 0=VIEW/1=RECORD/2=PREVIEW) を追加、既定 VIEW で後方互換。
- サーバー→クライアントに `BufferState { prefilling: bool, seconds: f32 }` 通知 (任意)。
- ロス通知 `StreamStats { pid_loss: ..., total_dropped: ... }` を低頻度で送る (デバッグ/クライアント表示用)。

---

## 11. 実装フェーズ

| Phase | 内容 | 依存 | 効果 | 状態 |
|-------|------|------|------|------|
| **P1** | per-PID CC ロス検出を配信経路へ接続 + ダッシュボード表示 | なし | まず現状のロスを見える化 (以降の判断根拠) | ✅ 実装済 (2026-07-03) |
| **P2** | ストリームクラス導入 + RECORD の無損失キュー/エラー化 | P1 | 「パケロス NG」を録画で担保 | ✅ 実装済 (2026-07-03)。BNDP v2 (Hello に StreamClass、v1 クライアントは View 扱いで互換)。RECORD は broadcast Lagged / 送出キュー 10s 超過で `record_broadcast_lag` / `record_queue_overflow` として切断・記録 |
| **P3** | 固定秒数プリフィル/ジッタバッファ (ビットレート連動) | P2 | 安定化・アンダーラン抑止 | ✅ 実装済 (2026-07-03) |
| **P4** | 共有エンコーダ(EncodePool) + 同時数セマフォ | なし(並行可) | tsreplace 複数ストリーム高速化 | ✅ 実装済 (2026-07-03)。プロファイル表は P5 に繰り延べ |
| **P5** | エンコードプロファイル表 + HTTP-TS エンドポイント + ダッシュボード mpegts.js プレビュー | P4 | ブラウザプレビュー | ✅ 実装済 (2026-07-04) |
| **P6** | Mirakurun 互換 API サブセット + WebSocket ダッシュボード | P5 | エコシステム連携 | Mirakurun 互換 API サブセットのみ ✅ 実装済 (2026-07-04)。WebSocket ダッシュボードは未着手 |

実装メモ (P1/P4):
- P1: 既存 `TsPacketAnalyzer` を per-PID 化 (上限 256 PID + overflow 集約)。CC 判定に duplicate 許容・discontinuity_indicator 抑止を追加。broadcast `Lagged(count)` の count はチャンク数であり `packets_dropped` に混ぜる従来動作は単位バグとして分離 (`loss_broadcast_lag_chunks`)。損失源別カウンタ + top PID を `/api/client/:id/quality`・ダッシュボード・`session_history.loss_summary`(JSON) に公開。
- P4: `tuner/encoder_pool.rs` 新設。EncodeKey = (channel_key, sids ソート済, config_generation=設定内容ハッシュ)。同一 key は permit 消費なしで合流。飽和時は生 TS passthrough。watchdog は「入力あり・出力 read_timeout 超停止」で kill → セッションは passthrough_on_error に従う。エンコーダのチューナー購読は subscriber_count 非加算 (`subscribe_untracked`) とし、チューナーの keep-alive 会計はセッション駆動を維持 (§5.2 からの意図的逸脱)。RECORD 優先確保は P2 のストリームクラス導入後に実装。

実装メモ (P3):
- `server/prefill.rs` 新設。`PrefillBuffer` は `Filling{queued, queued_bytes, target_bytes}` / `Passthrough` の2状態のみを持つ独立ステートマシン (Session 非依存、単体テスト可)。挿入位置は `send_ts_data_raw()` 内、BNDP ヘッダ付与直後・`send_ts_frame()` 直前 — プリフィル中のフレームはクラス別バックプレッシャ方針を一切通らない (RECORD の無損失オーバーフロータイマーもプリフィル解放後から起算)。
- サイズ決定: 実測ビットレートは使わず (§4.2 の将来課題として明記の通り、今回は非対応)、`current_nid` を `BandType::from_nid` で分類した帯域別デフォルト (地上波/不明 18Mbps・BS/CS 24Mbps・4K 33Mbps) を使用。v1 `SetChannel` (NID非解決) は「不明」扱い。
- 発動: `StartStream` 成功時、および `update_service_filter_for_sid`(全チャンネル解決経路が通る箇所) で `state==Streaming` の場合に `reset()`。`PurgeStream`/`StopStream` は `clear()`(キュー破棄のみ、Filling/Passthrough 状態は維持)。`prefill_ms=0` は即 `Passthrough` (完全バイパス)。
- アンダーラン後の再プリフィルは実装せず、全クラスで「枯渇後も流し続ける」(§4.3-4 の VIEW 挙動を全クラスへ拡張・明記の逸脱)。
- `tuner_config` に `prefill_view_ms`/`prefill_preview_ms`/`prefill_record_ms`/`jitter_safety_factor` を追加 (migration 009)。`TunerPoolConfig` には追加せず (チューナーのライフサイクル設定であり、プリフィルはセッション単位の出力バッファリングで無関係なため) — セッションは `load_tsreplace_runtime_config` と同様に `StartStream` 時に DB を直接読む。`SessionInfo.prefilling` を `/api/clients` とダッシュボードのクライアント一覧に追加。

※ P4 は他と独立に着手可能。プレビュー(P5)は P4 の H.264 プロファイルに乗るため P4 → P5 の順。

実装メモ (P5):
- `database/encode_profile.rs` 新設。`encode_profiles` テーブル (§5.3/§9) は `schema.rs` に
  `CREATE TABLE IF NOT EXISTS` で追加 (新規テーブルなので `add_column_if_not_exists` 型のマイグレーションは不要 —
  既存 DB でも `IF NOT EXISTS` がそのまま効く)。`Database::open()`/`open_in_memory()` 内で
  `seed_default_encode_profiles()` を毎回呼び、`name='preview-h264'` の行が無ければ
  H.264/~2Mbps/QSVEncC 引数テンプレートの行を1件 INSERT (べき等)。CRUD は
  `get_all/get/get_by_purpose/insert/update/delete`。API: `GET/POST /api/encode-profiles`,
  `POST/DELETE /api/encode-profiles/:id` (`web/api.rs`)。**`command_path` はテーブルにもリクエスト型にも存在しない**
  — 実行コマンドは引き続き `tsreplace_config.command_path` (TOML専用) のみ。
- **選局ロジックの共有 (§6.3)**: `handle_set_channel_space` (session.rs、~450行) はセッション状態
  (`current_tuner_path`/`group_driver_paths`/warm tuner 等) に強く依存し、仮想space変換・グループ選局の
  ドライバー横断探索・排他退避・容量フォールバックまで含む。これを丸ごと切り出すのはハイリスク
  (この環境では実機/実バイナリでの回帰確認ができない) と判断し、**`server/channel_resolve.rs` を新設して
  「`channels.id` (sid) → 自分自身の driver/space/channel を直接引く」経路のみを実装**した
  (`resolve_service` / `start_tuner_for_service`)。`channels` テーブルは各行が既に具体的な
  `(bon_driver_id, bon_space, bon_channel)` を持つため、HTTPの「特定サービスを1本再生したい」要求には
  仮想space変換もグループ横断探索も不要 — それらはセッションが「抽象インデックスで、複数ドライバーの
  どれか」を選ぶための機構であり、sid直指定には無関係。`ChannelKey`/`TunerPool::get_or_create`
  (factoryは no-op) → 未起動なら `SharedTuner::start_bondriver_reader` という**session.rs の
  単一チューナーモード分岐と全く同じ呼び出し列**を使うため、二重実装ではなく「同じ土台の上の専用経路」。
  グループ選局・容量フォールバック・他セッションの視聴中リーダーに対する優先度ベースの排他退避は
  意図的に非対応 (詳細はモジュール doc comment)。
  - **例外: 同一デバイスパス上の idle リーダー退避 (2026-07 追加)**: px4-drv 系キャラクタデバイス
    (`/dev/px4videoN` 等) は device path 単位で同時 1 `open()` しか許可せず、二重 open は
    `EALREADY` (errno 114) を返す。これは DB の `max_instances` 設定とは独立な物理制約であるため、
    keep-alive (`TunerPool::schedule_idle_close`, 既定60秒) で購読者ゼロのまま fd を握り続ける
    古いリーダーが同一パスに残っていると、別チャンネルへの新規 HTTP プレビュー要求が 503 で失敗し続ける
    (実際に発生したバグ)。対策として `TunerPool::evict_idle_on_path(tuner_path, except)`
    (`tuner/pool.rs`) を追加し、同一 `dll_path` 上の「稼働中 (`is_running()`) かつ購読者ゼロ
    (`!has_subscribers()`)」なリーダーを退避対象とする。`start_tuner_for_service` はこれを
    (a) `max_instances` に対する容量チェックで over capacity と判定した時点で能動的に呼び、
    (b) それでも `start_bondriver_reader` が `EALREADY` を返した場合の最終手段として再度呼び
    300ms 後に1回だけリトライする (Unix限定、`#[cfg(unix)]`)。**稼働中かつ購読者ありのリーダーは
    一切退避しない**ため、他セッション/リクエストが実際に視聴中のストリームを奪うことはなく、
    session.rs の優先度ベース退避とは区別される。現在の `exclusive` は無条件の強制権ではなく、
    client claim の `(priority, exclusive)` 辞書式比較のタイブレークであり、同順位の exclusive
    同士は先着のストリームを維持する。
    容量不足のまま退避対象が見つからない場合は `ChannelResolveError::Busy` → HTTP 503。
- **切断時のリーク防止**: `web/stream.rs::StreamCleanup` が RAII ガード。レスポンスボディ
  (`axum::body::Body::from_stream` + `futures::stream::unfold`) の state に埋め込み、
  ストリームが (正常終了・クライアント切断どちらでも) drop されたら `Drop` 内で `tokio::spawn` して
  非同期クリーンアップ (`SharedTuner::unsubscribe` → 購読者0なら `TunerPool::schedule_idle_close`、
  `profile=preview` なら追加で `EncoderPool::release`) を行う。`Drop::drop` は同期関数なので await できず、
  detached task に逃がす形。**この切断シナリオ自体は実機ブラウザ/クライアントでの実行時検証ができていない**
  (b25-sys リンク不可のためフルバイナリが動かせない) — 単体テストでは `StreamCleanup` を直接 drop して
  `SharedTuner::subscriber_count` が減ることのみ確認 (`web/stream.rs` の
  `dropping_stream_cleanup_releases_tuner_subscription`)。
- **broadcast Lagged の扱い**: HTTP ストリームは PREVIEW 相当のポリシーで、`Lagged` はログのみで継続
  (§3.2/§3.3 の VIEW/PREVIEW 方針を踏襲)。RECORD 相当の「溢れたら切断」は HTTP エンドポイントには適用していない
  (Mirakurun 風 HTTP 配信は §2 の意味では PREVIEW/VIEW 用途を想定)。
- **profile=preview の SID 選択**: `/api/stream/service/:sid` は1サービス指定なので、
  `encoder_pool::args_contain_service_option` が偽なら常に `sids=[その1 SID]` でエンコード
  (session.rs の single-service モードと同じ規約)。`tsreplace_config.enabled=false` の場合は
  503 で明示エラーを返す (実行コマンド自体を無効化している設定なので、preview だけ黙って動かすのは矛盾するため)。
- **生 passthrough の範囲**: `profile` 未指定時はチューナーの生 TS (物理チャンネルのフル multiplex) を
  そのまま流す。session.rs のようなサービスPIDフィルタ (`ts_analyzer::service_filter`) は適用していない
  — 実装コスト・回帰リスクに対して仕様上の要求 (§6.3 は「生 TS」としか書いていない) を超えないための意図的な
  簡略化。地上波 (MPEG-2) は主要ブラウザで再生できない制約 (§6.2) は passthrough 経路にも変わらず適用される。
- **mpegts.js の同梱**: `mpegts.js` を `web-ui/package.json` の依存として管理し、ViteがVue成果物へバンドルする。
  CDNフォールバックと `/static/mpegts.js` は廃止したため、配布物は完全に同一オリジンかつオフラインで完結する。
  認証ヘッダは `PreviewPlayer.vue` が `config.headers` 経由で `Authorization: Bearer <token>` を付与する。
  実ストリームを使った再生確認のみ、チューナーとエンコーダを利用できる環境で実施する。

実装メモ (P6):
- `web/mirakurun.rs` 新設。`/mirakurun/api/*` に `GET version/status/channels/services`,
  `GET services/:id/stream`, `GET channels/:type/:channel/stream` を実装 (`web/mod.rs::build_mirakurun_router`)。
- **マウント先・認証・opt-in**: `/api/*` とは別ルータとして `/mirakurun/api` にマウントし、
  `require_auth` を掛けない。理由は実 Mirakurun クライアント (EPGStation/mirakc/KonomiTV) が
  `Authorization` ヘッダを送らないこと、かつ Mirakurun の実パス (`/api/services` 等) がこのプロジェクト
  自身のダッシュボード API (`/api/channels` 等) と衝突すること。**既定 disabled** の opt-in
  (`[mirakurun] enabled = false`、`main.rs::MirakurunSection`)。`build_app`/`start_web_server` は
  `mirakurun_enabled: bool` を受け取り、`true` の時だけ `.nest("/mirakurun/api", ...)` する
  (`false` なら 404 — ルータに存在しない)。有効化時は `start_web_server` が起動時に一度だけ
  「無認証で配信される、信頼ネットワーク/localhost のみで公開せよ」という WARN を出す。`web_listen` の
  既定 `127.0.0.1` と合わせ、二重に既定安全。
- **P5 配信ロジックの再利用**: `web/stream.rs` の `StreamCleanup`(RAII、`tuner_only` コンストラクタを追加)・
  `broadcast_to_body_stream`・`respond_with_stream`・`error_response`・`channel_resolve_error_response`
  (`NotFoundNidSid` variant 追加に伴い引数を `sid: i64` から `context: impl Display` に一般化) を
  `pub(crate)` 化し、`web/mirakurun.rs` から直接呼び出す形で再利用した(二重実装なし)。
  `server/channel_resolve.rs` は `resolve_channel_record` という共通の検証本体を切り出し、
  既存の `resolve_service(db, channels.id)` と新設の `resolve_service_by_nid_sid(db, nid, sid)`
  (Mirakurun の `id = nid*100000+sid` を分解した後の解決に使用) の双方から呼ぶ形にリファクタ。
  `database/channel.rs` に `get_channel_by_nid_sid` を新設 (`tsid` 抜きで `(nid, sid)` のみで引く。
  複数一致時は `is_enabled DESC, priority DESC, id ASC` で決定的に1件選ぶ)。
  Mirakurun 側のストリームは **passthrough 固定** (生 TS、`?profile=` 等の変換クエリは実装しない —
  §7.1 の「passthrough が既定」の通り)。
- **複数候補への対応 (2026-08 追加)**: `channels` テーブルは (BonDriver, サービス) ごとに1行のため、
  同じサービスが複数の BonDriver に載っていることがある (実例: 実サーバーで SID 21520 が3行 —
  他ドライバは満杯だが別の1台では既に稼働中だった)。これまで `resolve_service_by_sid`/
  `resolve_service_by_nid_sid` は `LIMIT 1` で先頭行しか返さず、`tuner::acquire::acquire` に
  単一候補しか渡していなかったため、その1行のドライバが埋まっていると他ドライバで既に配信中でも
  503 になっていた。`database/channel.rs` に `get_channels_by_sid`/`get_channels_by_nid_sid`
  (`LIMIT` なし、既存単数版と同じ `ORDER BY is_enabled DESC, priority DESC, id ASC`) を追加し、
  `ResolvedService` を「代表行 `channel` + 全候補 `candidates: Vec<CandidateTarget>`」の形に変更。
  `resolve_service_by_sid`/`resolve_service_by_nid_sid` は enabled な行それぞれを物理ターゲットへ解決し
  (`(dll_path, space, channel)` で重複排除)、`start_tuner_for_service` は全候補の `channel_key` を
  `acquire()` に渡す。ドライバ選択・既存リーダーへの相乗り (`Decision::Reuse`) は変わらず
  `tuner::policy::decide` の仕事——本モジュールは候補リストを組み立てるだけで、`decide`/`acquire`
  自体は変更していない。`resolve_service`(`channels.id` 直指定) は従来どおり単一候補のまま
  (`channels.id` は「この行のこのドライバ」を明示的に指す契約のため広げない)。
  `ChannelResolveError::Busy` は `drivers: usize` フィールドを追加し「全何台の候補ドライバが
  埋まっているか」を伝える (`running`/`max`/`path` は先頭候補=最高優先度候補のもの)。
  `acquire()` が実際にどの候補を選んだかは `AcquireOutcome::key`(= 起動/合流した `SharedTuner::key`)
  でのみ分かるため、`web/stream.rs::session_info_for` はダッシュボードのクライアント一覧に出す
  `tuner_path`/`channel_info` を `resolved`(先頭候補の代表行)からではなく実際の `SharedTuner::key`
  から組み立てるよう変更した。`EncodeKey`(§5.3 プレビュー用) も同様に実キーを使う
  ——直さないと、別ドライバに載った同一サービスのプレビューがエンコーダ共有可否を誤判定する。
- **データマッピング**: サービス id は `mirakurun_service_id(nid, sid) = nid as u64 * 100000 + sid as u64`
  とその逆変換 `split_mirakurun_service_id`(範囲外は `None`)。`band_type → Mirakurun type` は
  `Terrestrial→"GR"`, `BS→"BS"`, `CS→"CS"`, `FourK→"BS"`(4K相当の型が本サブセットに無いための簡略化),
  `CATV→"GR"`, `SKY`/`Other→"SKY"`(いずれも本来の Mirakurun 型に忠実な対応が無いための便宜的バケツ)。
  `GET /channels/:type/:channel/stream` 用の逆引き (`mirakurun_type_to_band_candidates`) は必然的に
  一対多 (`type=BS` は `BandType::BS`/`FourK` の両方にマッチ)。`channel` 文字列は地上波は `physical_ch`
  優先(無ければ `bon_channel`)、それ以外は `bon_channel` をそのまま10進文字列化 — 実 Mirakurun の
  衛星チャンネル表記 (`"BS15_0"` 等のトランスポンダ+スロット形式) は再現していない
  (EPGStation/KonomiTV は主に `id`/`serviceId` で参照するため、視聴が通ることを優先した簡略化)。
  `/channels`・`/services` は `is_enabled` かつ `bon_channel` が設定済み (スキャン済み) の行のみ列挙。
  ロゴ (`logoId`/`hasLogoData`) は常に「無し」を返す (`hasLogoData: false`) — 未実装。
  EPG (`/api/programs` 等) はスコープ外のまま。
- **実行時未検証**: この環境では実機 BonDriver・実 Mirakurun クライアント (EPGStation/mirakc/KonomiTV) の
  いずれも動かせない (b25-sys の C++ リンク不可)。カバレッジは in-memory DB に対するユニット/統合テスト
  (`cargo test`、`tower::ServiceExt::oneshot`) のみ — サービス id 往復、`BandType` 正引き/逆引きの
  一対多整合性、`channel` 文字列導出、`/mirakurun/api/version` が無認証で 200 を返すこと、
  `enabled=false` 時に `/mirakurun/api/*` が 404 になること、`/services`/`/channels` が
  in-memory DB のシードデータから妥当な JSON を返すことを確認。EPGStation のサービス検出・KonomiTV の
  ライブ視聴が実際に通るかどうかは未検証。

---

## 12. 未解決の論点 (要判断)

1. **RECORD の溢れ時**: 切断(明確)か、一時的にディスクへスピル(無損失だが複雑)か。まずは切断+記録を推奨。
2. **プレビューのコーデック**: H.264(互換最優先) 固定か、HEVC も許容(対応ブラウザ限定)か。既定 H.264 を推奨。
3. **WASM デコード(ts-live 方式)**の採否: MPEG-2 を変換せず出せるが負荷大。当面は「変換プレビュー」を本命にし、
   ts-live 方式は将来オプション。
4. **クライアント DLL の固定 19.2MB リング**を秒数指定に変える際、TVTest 側の取り出し前提を壊さないか要検証。

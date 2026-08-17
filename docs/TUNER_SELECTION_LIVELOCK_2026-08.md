# チューナー選択の livelock と設計方針 (2026-08-17)

本番機 `fuku-recisdb-web.stuayu.com` で「視聴・録画ができない」事象が発生した。
稼働中サーバーのログ API (`/api/logs`, `/api/clients`, `/api/tuners`,
`/api/channels/export`) から実データを取得して原因を特定した記録と、その修正方針。

`docs/TUNER_PIPELINE_REDESIGN.md` の P2b-3 で「退避ポリシーを統一した」と
された箇所に、統一しきれていない前提の食い違いが残っていた。本書はその
続き (P5) として扱う。

---

## 1. 観測された事象

2026-08-17 17:29〜17:33 の稼働ログ (in-memory リングバッファ) より。

### 1.1 セッションの回転

| 指標 | 実測値 |
|---|---|
| 92 秒間に生成された distinct セッション | 204 (session_id 7788 → 8003) |
| うち SetChannelSpace 成功 | 118 |
| うち「退避された」警告 | 118 |
| うち AtCapacity 拒否 | 78 |
| 累計セッション数 (`/api/stats`) | 270,715 |

**選局に成功したセッションの実質 100% が、直後に他のセッションによって
退避されている。** 退避されたセッションは切断され、クライアント
(BonDriverProxyEx、すべて `127.0.0.1` の同一ホスト `DESKTOP-CN518N1`) が
即座に再接続し、また誰かを退避する。毎秒約 2.2 セッションの再接続ストーム。

### 1.2 退避の実際の並び

`BonDriver_PX4-T3_PE5.dll` (max_instances=2) 上で、物理 ch12 / ch14 / ch16 の
3 チャンネルが 2 スロットを奪い合い続けている。

```
17:29:57.169 create key=<PX4-T3_PE5 s0c14> evict=[<PX4-T3_PE5 s0c12>] priority=9  exclusive=false
17:29:57.169 WARN evicting <PX4-T3_PE5 s0c12> with 2 live subscriber(s)
17:29:58.994 create key=<PX4-T3_PE5 s0c12> evict=[<PX4-T3_PE5 s0c16>] priority=10 exclusive=true
17:29:58.994 WARN evicting <PX4-T3_PE5 s0c16> with 2 live subscriber(s)
17:29:59.568 create key=<PX4-T3_PE5 s0c16> evict=[<PX4-T3_PE5 s0c14>] priority=9  exclusive=false
17:29:59.568 WARN evicting <PX4-T3_PE5 s0c14> with 2 live subscriber(s)
17:30:01.326 create key=<PX4-T3_PE5 s0c14> evict=[<PX4-T3_PE5 s0c12>] priority=9  exclusive=false
17:30:01.326 WARN evicting <PX4-T3_PE5 s0c12> with 2 live subscriber(s)
```

12 → 14 → 16 → 12 → … と巡回するだけで、誰も安定して視聴できない。
80 秒間で「視聴者がいるチューナーの退避」が 63 回。

注目すべきは **`priority=9 exclusive=false` の要求までもが、視聴者 2 名を
抱えた稼働中チューナーを退避できている** こと。これは仕様ではない。

---

## 2. 根本原因

### 2.1 P0 — 優先度比較が非対称 (livelock の直接原因)

`tuner/policy.rs::may_evict` は要求側と居座り側の優先度を比較する:

```rust
fn may_evict(req: &TuneRequest, victim_priority: i32, victim_is_keep_alive: bool) -> bool {
    if victim_is_keep_alive { return true; }
    req.priority > victim_priority || req.exclusive
}
```

比較している 2 つの値の出自が違う。

- **`req.priority`** — `server/session.rs:1956` で決まる **クライアント優先度**。
  BNDP の `SetChannelSpace` が運んでくる値をそのまま使う
  (`if priority > 0 { priority }`)。本番の実測値は 9 と 10。

- **`victim_priority`** — `tuner/acquire.rs:267` の `snapshot()` が入れる
  **DB のチャンネル優先度** (`channels.priority`)。
  `db.get_channel_priority(path, space, channel).unwrap_or(Some(0)).unwrap_or(0)`。

そして本番 DB の `channels.priority` は **全 1637 行が 0**
(`/api/channels/export` で確認)。

つまり `may_evict` は常に `9 > 0` / `10 > 0` を評価している。
**優先度 1 以上を名乗るクライアントは、例外なく、あらゆる稼働中チューナーを
退避できる。** 優先度による保護がまったく機能していない。

P2b-3 のコメントは「`>=` をやめて `>` にしたので同順位では退避しない」と
書いているが、実際には同順位の比較が一度も成立していない。居座り側の
クライアント優先度がどこにも記録されていないためである
(`SharedTuner` は `subscriber_count: AtomicU32` しか持たず、購読者が誰で
どの優先度かを知らない)。

これが livelock の直接原因。全員が全員を退避できるので、定常状態が存在しない。

### 2.2 P1 — `exclusive` が無条件の切り札

`may_evict` の `|| req.exclusive` により、**exclusive 要求は優先度を一切問わず
退避できる**。優先度 1 の exclusive 要求が優先度 200 の録画を蹴り出せる。

本番では `client_exclusive: true, client_priority: 10` のクライアントが常時
接続しており (`/api/clients`)、この経路が実際に発火している
(`priority=10 exclusive=true` の evict ログ)。

exclusive 同士がぶつかった場合の規定もない。両者が互いを退避できる。

### 2.3 P2 — 保持直後のチューナーが即座に奪われる

最短で **1.8 秒**で退避されている (17:29:57.169 に作られた ch14 が
17:29:59.568 に退避)。TS が流れ始める前に奪われるため、クライアント側からは
「選局は成功したのに映らない」に見える。

チューナーを掴んでからの経過時間はポリシーの入力になっていない。

### 2.4 P3 — 拒否か退避かが運任せ

`policy.rs::eviction_options` は退避候補を `e.is_running()` で絞る。一方
容量計算は `e.occupies_slot()` (= `Reserved | Starting | Running | Stopping`)。
このズレにより、両スロットが `Reserved`/`Starting` の瞬間には「満杯だが
退避候補ゼロ」となり `AtCapacity { lowest_idle_priority: None }` で拒否される。
数百ミリ秒後に同じ要求を出すと今度は `Running` になっていて退避が通る。

ログ上の「拒否 78 / 退避 118」の混在はこれ。同一条件の要求に対して結果が
非決定的で、利用者からは「たまに映る」としか見えない。

なお「開始中のリーダーを退避候補にしない」判断自体は正しい (開いている
最中の BonDriver を殺すのは危険)。問題は、その状態が拒否理由として
区別されず、リトライすれば通ってしまう点にある。

### 2.5 P4 — 無データのままスロットを握り続けるセッション

`/api/clients` に以下が存在した:

```
session 15  host DESKTOP-CN518N1  prio 9  excl false
            tuner BonDriver_PX4-T1.dll  Space 0, Ch 2  ＮＨＫ総合１・福島
            connected 4028s  is_streaming true  current_bitrate_mbps 0.0
```

**67 分間、TS が 1 バイトも流れていないのに `PX4-T1` のスロットを保持し続けている。**
`SharedTuner`/セッションのどちらにも「一定時間 TS が来なければ切る」監視がない
(`read_timeout_ms` は BonDriver の 1 回の read に対するもので、
配信の枯死は見ていない)。

この結果 `PX4-T1` は残り 1 スロットとなり、そこへの新規 open は
`OpenTuner failed - tuner may be in use` で失敗、
`tuner/open_backoff` が最大 30 秒のクールダウンに入る。地上波の実効容量が
半減し、2.1 の livelock を発火させる過負荷状態が作られた。

### 2.6 (運用) PX-MLT1 の地上波チャンネルが無効化されている

`Main` グループは 5 ドライバ (PX-MLT1 max=4, PX4-S1 max=2, PX4-S3_PE5 max=2,
PX4-T1 max=2, PX4-T3_PE5 max=2)。しかし選局ログの候補数は一貫して
`candidates=2` (PX4-T1 と PX4-T3_PE5 のみ)。

`/api/channels/export` を見ると、**PX-MLT1 の地上波 22 行のうち 14 行が
`is_enabled=false`**。無効なのは ch12 (NID 32418) / ch13 (32421) /
ch14 (32419) / ch16 (32420) — まさに奪い合っている 3 波を含む。有効なのは
NHK 系の ch1 / ch2 のみ。

`server/session_channel_candidates.rs:36` の候補収集は `ch.is_enabled` で
除外するため、**PX-MLT1 の 4 スロットは選局候補として一切見えていない。**
空いている 4 スロットの隣で、2 スロットを奪い合っていたことになる。

これはコードのバグではなく DB の状態だが、`is_enabled` が
「クライアントのチャンネル一覧に出さない」と「選局候補にしない」の
2 つの意味を兼ねていることによる事故である。前者は
`server/client_view.rs` が (NID,TSID) で重複排除するため、そもそも
無効化しなくても重複表示されない。

---

## 3. 修正方針

### P0. 優先度比較を対称にする (必須・これだけで livelock は止まる)

**居座り側のクライアント優先度を `SharedTuner` に記録し、それと比較する。**

1. `SharedTuner` に「claim (占有主張)」の台帳を持たせる。
   - `claims: Mutex<HashMap<ClaimId, Claim>>`、`Claim { priority: i32, exclusive: bool }`。
   - `subscribe()` を `subscribe_with_claim(priority, exclusive)` に拡張し、
     返す `TunerSubscription` の `Drop` で claim を外す。
     購読者数の増減と claim の増減が必ず一致するので、漏れが構造的に起きない。
   - `subscribe_untracked()` (encoder_pool の寄生購読) は claim を作らない。
     現状の「購読者数に数えない」契約と揃える。
   - 集約 API: `fn incumbent_claim(&self) -> Option<Claim>` — 全 claim の
     `(priority, exclusive)` の最大値。

2. `policy::EntryState` の `priority` の意味を変える。
   - `acquire.rs::snapshot()` で
     `priority = max(db_channel_priority, incumbent_claim.priority)` を入れる。
   - `EntryState` に `incumbent_exclusive: bool` を追加。
   - DB のチャンネル優先度は「誰も見ていないチューナーの下限」として残す
     (現在すべて 0 なので実質 claim 側が支配する)。

3. `may_evict` を、同じ物差しの比較にする。

**注意:** `EntryState.priority` の意味変更は `policy.rs` の既存テストの前提を
変える。テストは「DB 優先度」を渡している箇所が多いので、
`incumbent_exclusive` の追加と合わせてテストを更新すること。

### P1. `exclusive` をタイブレークに格下げする

要求側・居座り側とも `(priority, exclusive)` の辞書式順序で比較し、
**要求側が厳密に大きいときだけ退避を許す**。

```rust
fn claim_rank(priority: i32, exclusive: bool) -> (i32, u8) {
    (priority, exclusive as u8)
}

fn may_evict(req: &TuneRequest, victim: &EntryState, victim_is_keep_alive: bool) -> bool {
    if victim_is_keep_alive {
        return true; // keep-alive の残り火は最適化にすぎない。同順位でも譲る
    }
    claim_rank(req.priority, req.exclusive)
        > claim_rank(victim.priority, victim.incumbent_exclusive)
}
```

これにより:
- 同優先度・同 exclusive → 退避不可 (先着優先)。livelock が止まる。
- 同優先度で要求側だけ exclusive → 退避可 (「ハードウェアを寄越せ」の意図を尊重)。
- 低優先度の exclusive が高優先度の録画を蹴る経路が消える。
- exclusive 同士の相互退避が消える。

`keep_alive` の分岐は現状のまま維持する (`policy.rs::may_evict` の
既存コメントの理由がそのまま有効)。

### P2. 最低保持時間 (grace) を入れる

P0+P1 で同順位の巡回は止まるが、優先度が本当に異なる 2 者
(録画 200 vs 録画 255 など) が交互に来ると依然フラップしうる。

- `SharedTuner` に `running_since: Option<Instant>` (`ReaderState::Running`
  遷移時刻) を持たせ、`EntryState` に `held_for: Duration` として載せる。
- `held_for < min_hold` の稼働中チューナーは、**購読者がいる限り退避不可**
  (`may_evict` が無条件 false)。要求側は `AtCapacity` で拒否される。
- 購読者ゼロ (keep-alive の残り火) には適用しない。ザッピング復帰の最適化を
  壊さないため。
- 設定値 `min_hold_secs` (既定 10) は **`tuner_config` テーブル**に追加する
  (TOML ではない。keep_alive_secs と同じ場所)。
  `database/mod.rs` の `MIGRATIONS` 台帳に追記し、**冪等**にすること。
  `/api/tuner-config` (GET/POST, `web/api/configs.rs`) と Web UI 設定画面にも出す。

### P3. 拒否理由を状態で区別する

`RejectReason::AtCapacity` を分割する。

- `AtCapacity { lowest_idle_priority }` — 退避候補はあるが優先度が足りない。
  クライアントに「優先度不足」と伝えられる。
- `Warming { retry_after: Duration }` (新設) — 全スロットが
  `Reserved`/`Starting` で、退避候補が原理的に存在しない状態。
  短時間待てば解消する。

`eviction_options` は `is_running()` のままでよい (開始中を殺さない判断は正しい)。
ただし `decide_at_capacity` で「`occupies_slot()` は満杯だが `is_running()` の
候補がゼロ」を検出したら `Warming` を返す。

`acquire.rs` はこれを `AcquireError::Warming` として上げ、`session.rs` は
ERROR ではなく INFO でログし、クライアントには再試行可能であることを示す
エラーコードを返す。ログの ERROR 洪水 (80 秒で 104 行) も止まる。

### P4. 再接続ストームを入口で抑える

204 セッション/92 秒は、ポリシーが正しくなっても叩かれてよい頻度ではない。

- `tuner/open_backoff.rs` と同じ形で、`(client host, candidate set)` 単位の
  **拒否クールダウン**を持つ。
- `acquire` が `AtCapacity`/`Warming` を返したら記録し、同一クライアントから
  同一チャンネルへの要求が `reject_cooldown_ms` (既定 2000) 以内に再来したら、
  `Reuse` 判定を先に通したうえで、キャッシュ済みの拒否を返す。Reuse の可能性を
  潰さないため、拒否ゲートは判定後に適用する。
- ログは `open_backoff` と同様に間引く (1 分あたり 1 行 + 抑制件数)。
- 成功した選局はクールダウンを解除する。

### P5. 無データセッションの回収

- `SharedTuner` の読み取りループに **TS 枯死ウォッチドッグ**を入れる。
  `Running` に入ってから `no_data_timeout_secs` (既定 30) の間、
  broadcast に 1 バイトも送出できなかったら
  `StopReason::ReaderFailed` で自らリーダーを停止し、スロット permit を解放する。
- 既存の displaced 検出 (`session.rs:849` の `reader_state_rx` watch) が
  そのまま発火するので、セッション側の追加処理は不要。
- 設定値も `tuner_config` に置く (P2 と同じマイグレーションにまとめてよい)。

**不変条件に注意:** リーダーの読み取りループは「読む・B25 デコード・配る」
のみという CLAUDE.md の制約がある。ウォッチドッグはプロセス起動時の
`Instant` を基準にした単調経過ミリ秒の `AtomicU64` を Running 遷移時と
TS 読み出し後に更新し (購読者ゼロでの broadcast 失敗も含む)、判定は
ループ外の別タスクで行うこと。システム時刻の巻き戻しに依存してはならない。

---

## 4. 運用側の対応 (コード変更ではない)

### 4.1 PX-MLT1 の地上波チャンネルを有効化する

`is_enabled=false` の 14 行を有効化すると、地上波の選局候補が 2 → 3 ドライバ、
実効スロットが 2 → 6 に増える。§2.6 の状況では最も効く単発の対処。

**影響:** チャンネル列挙順は (NID,TSID) で重複排除されるため、
PX-MLT1 の行を有効化しても表示上のチャンネルは増えない
(既に PX4-T1/T3 が同じ (NID,TSID) を持っている)。よって
**.ch2 / ChSet の再生成は不要**。ただし実行前に
`/api/client-view` の出力差分で確認すること。

### 4.2 PX4-T1 の枯死セッションを切る

`POST /api/client/15/disconnect` でスロットを解放し、
`open_backoff` のクールダウンを解消させる。P5 実装後は自動回収される。

---

## 5. 実装順序と検証

| 段階 | 内容 | 効果 |
|---|---|---|
| P0 | 優先度比較の対称化 | livelock 停止 (必須) |
| P1 | exclusive のタイブレーク化 | 低優先 exclusive による録画破壊の防止 |
| P2 | 最低保持時間 | 優先度差がある場合のフラップ防止 |
| P3 | 拒否理由の分離 | 非決定的な挙動の可視化、ERROR 洪水の停止 |
| P4 | 再接続ストーム抑制 | 負荷とログ量の削減 |
| P5 | 無データセッション回収 | 過負荷状態そのものの解消 |

### 検証

```powershell
cargo test -p recisdb-proxy
cargo build -p recisdb-proxy
```

`tuner/policy.rs` に以下の回帰テストを追加すること。

1. **livelock 再現テスト** — max_instances=2 のドライバに ch12/ch14 が
   それぞれ購読者付きで稼働中。priority=9 exclusive=false で ch16 を要求。
   → `Reject`。修正前は `Create { evict: [ch12] }` になる。
2. **同優先度 exclusive 同士** — 居座り (prio 10, exclusive) に対して
   (prio 10, exclusive) の要求 → `Reject`。
3. **低優先 exclusive vs 高優先録画** — 居座り (prio 200, 非 exclusive) に
   対して (prio 10, exclusive) の要求 → `Reject`。
4. **正当な昇格** — 居座り (prio 9) に対して (prio 200) の要求 → `Create { evict }`。
5. **keep-alive の残り火は同順位でも譲る** — 既存の挙動が維持されること。
6. **grace 期間中は退避されない** — `held_for < min_hold` かつ購読者ありの
   居座りは、より高い優先度の要求でも `Reject`。
7. **全スロットが Starting** — `Warming` が返ること。

実機での確認は BonDriver DLL が必要なため、テストは DB/ロジック層に限定される。
本番反映後は `/api/logs?level=warn` で「evicting ... with N live subscriber(s)」が
消えることと、`/api/clients` の `connected_seconds` が伸び続けること
(= セッションが回転しなくなったこと) を確認する。

---

## 6. 関連ドキュメント

## 7. 実装結果

P0〜P5 の方針に沿って、claim 台帳、辞書式退避比較、最低保持時間、Warming 拒否、
DB/Web/UI 設定、拒否クールダウン、無データ watchdog と回帰テストを実装した。
実装では watchdog の判定を async 別タスクに置き、読み取りループには最終送出時刻の
Atomic 更新だけを残した。既存の共有 HTTP 購読は stateless のため host claim は
`http` として扱う。P2 の設定値は DB/API から pool に反映される。

- `docs/TUNER_PIPELINE_REDESIGN.md` — 本書はその P5 にあたる。
  §2.1-8 の「退避ポリシーの不整合」は P2b-3 で統一されたとされていたが、
  比較する値の出自が揃っていなかった。同 §を本書へのポインタ付きで更新すること。
- `docs/STREAMING_DESIGN.md` — §2 の優先度と stream_class の関係。
  P1 で exclusive の意味が変わるため、該当箇所を更新すること。
- `CLAUDE.md`「選局(チューナー選択)」節 — 決定は `policy.rs::decide` のみ、
  実行は `acquire.rs::acquire` のみ、という不変条件は維持する。

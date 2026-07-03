# クライアント接続後の処理シーケンス図

## 概要

新規クライアントがチューナーを開いてチャンネルを選択し、TSストリームを受信するまでの処理フローと、
チューナー上限到達時に発生していたバグの修正内容。

---

## 1. 通常フロー（チューナー空きあり）

```text
  Client              Session (Server)         TunerPool          BonDriver DLL
    |                        |                      |                    |
    |── TCP Connect ─────────▶                      |                    |
    |◀── Hello ──────────────|                      |                    |
    |── HelloAck ────────────▶                      |                    |
    |                        |                      |                    |
    |── OpenTuner(DLL) ──────▶                      |                    |
    |                        |── keys() ───────────▶|                    |
    |                        |◀─ [] (未使用) ────────|                    |
    |                        |  [DLL未使用の場合]    |                    |
    |                        |────────────────────────────── WarmTuner起動▶|
    |                        |  [DLL使用中の場合]    |                    |
    |                        |  already_running=true → ウォームチューナースキップ
    |◀── OpenTunerAck ───────|                      |                    |
    |                [state: TunerOpen]             |                    |
    |                        |                      |                    |
    |── GetSpace ────────────▶                      |                    |
    |◀── GetSpaceAck ─────────|                      |                    |
    |── GetChannel ──────────▶                      |                    |
    |◀── GetChannelAck ───────|                      |                    |
    |                        |                      |                    |
    |── SetChannelSpace ─────▶                      |                    |
    |   (space, ch,          |                      |                    |
    |    priority,           | map_space_idx_to_actual()                 |
    |    exclusive)          | ensure_channel_map()                      |
    |                        |                      |                    |
    |                        |  ┌─ 同一チャンネル再利用チェック ──────────┐  |
    |                        |  │ keys() でプールを検索                  │  |
    |                        |  │  ・同一チャンネルが is_running=true    │  |
    |                        |  │    → cancel_idle_close()              │  |
    |                        |  │    → stop_warm_tuner()                │  |
    |                        |  │    → current_tuner = 既存リーダー      │  |
    |                        |  │    → 即時返却 ✓                       │  |
    |                        |  │  ・見つからない場合 → 新規作成へ        │  |
    |                        |  └────────────────────────────────────────┘  |
    |                        |                      |                    |
    |                        |  [新規の場合]         |                    |
    |                        |── get_or_create() ──▶|                    |
    |                        |◀─ new SharedTuner ───|                    |
    |                        |────────────── start_reader_with_warm() ──▶|
    |                        |               (SetChannel, TS待機, B25初期化)
    |                        |◀────────────────── ready ─────────────────|
    |◀── SetChannelSpaceAck ──|                      |                    |
    |   (success=true)       |                      |                    |
    |                        |                      |                    |
    |── StartStream ─────────▶                      |                    |
    |                        |── cancel_idle_close() ▶                   |
    |                        |── subscribe() ───────▶|                    |
    |                        |◀─ broadcast::Receiver |                    |
    |                        |  subscriber_count += 1|                    |
    |◀── StartStreamAck ──────|                      |                    |
    |               [state: Streaming]              |                    |
    |                        |                      |                    |
    |         ╔══════════════════ TSデータ配信ループ ══════════════════╗   |
    |         ║               |                      |                ║  |
    |         ║               |◀── broadcast(chunk) ──────────────────║──|
    |         ║               |  recv() → Ok(data)   |                ║  |
    |◀── TS data ─────────────║───|                  |                ║  |
    |         ║               |                      |                ║  |
    |         ╚══════════════════════════════════════════════════════╝   |
    |                        |                      |                    |
    |── StopStream ──────────▶                      |                    |
    |                        |── unsubscribe() ─────▶|                    |
    |                        |  subscriber_count -= 1|                    |
    |                        |  [count==0 の場合]    |                    |
    |                        |── schedule_idle_close(60s) ▶              |
    |◀── StopStreamAck ───────|                      |                    |
    |               [state: TunerOpen]              |                    |
```

---

## 2. チューナー上限到達時 — 修正前（バグあり）

**条件:** `max_instances=1`、Channel X が Session A でストリーミング中、
Session B が同じ Channel X を `exclusive=true` でリクエスト。

```text
  Client B (新規)      Session B            TunerPool         Session A (配信中)
       |                    |                    |                    |
       |                    |      ◀── Channel X: is_running=true ───|
       |                    |          subscriber_count=1             |
       |                    |                    |                    |
       |── OpenTuner ───────▶                    |                    |
       |                    |── keys() ──────────▶                    |
       |                    |  already_running=true → ウォームチューナーなし
       |◀── OpenTunerAck ───|                    |                    |
       |                    |                    |                    |
       |── SetChannelSpace ─▶                    |                    |
       |   (X, exclusive=true)                  |                    |
       |                    |                    |                    |
       |       ╔════════════╧════════════════════╧════════╗          |
       |       ║  ❌ [BUG] exclusive ブロック (行 2241)    ║          |
       |       ║                                          ║          |
       |       ║  running_on_dll=1 >= dll_max=1           ║          |
       |       ║  best_idle = None  (サブスクライバあり)   ║          |
       |       ║  best_any  = Channel X  ← ❌ 選択されてしまう        |
       |       ║                                          ║          |
       |       ║  stop_reader(Channel X) ─────────────────╫──────────▶|
       |       ║  remove(Channel X)                       ║   ❌ 強制停止
       |       ╚════════════╤════════════════════╤════════╝   Stream切断
       |                    |                    |                    |
       |                    |  同一チャンネル再利用チェック             |
       |                    |  → Channel X が見つからない（削除済み）  |
       |                    |                    |                    |
       |                    |  容量チェック: running=0 < max=1 → OK   |
       |                    |── get_or_create(Channel X) ──▶          |
       |                    |── start_reader_with_warm() ──▶          |
       |                    |   (SetChannel… 数秒待機)                |
       |◀── SetChannelSpaceAck |                  |                    |
       |    (success=true)  |                    |                    |
       |── StartStream ─────▶                    |                    |
       |                    |── subscribe() ──────▶                    |
       |◀── StartStreamAck ─|                    |                    |
```

**問題点まとめ:**

| 項目 | 内容 |
|---|---|
| バグ箇所 | `session.rs` 行 2241 — `exclusive` 退避ブロック |
| 原因 | 退避ブロックが同一チャンネル再利用チェック（行 2300）より**前**に実行される |
| `best_any` の問題 | サブスクライバの有無を問わず全チューナーを退避候補にする |
| 結果 | 再利用できたはずのチャンネルを停止し、不要なリーダー再起動が発生 |

---

## 3. チューナー上限到達時 — 修正後（正しい動作）

**修正:** 退避前に「要求チャンネルが既に動作中か」を確認するチェックを追加。

```text
  Client B (新規)      Session B            TunerPool         Session A (配信中)
       |                    |                    |                    |
       |                    |      ◀── Channel X: is_running=true ───|
       |                    |          subscriber_count=1             |
       |                    |                    |                    |
       |── OpenTuner ───────▶                    |                    |
       |◀── OpenTunerAck ───|                    |                    |
       |                    |                    |                    |
       |── SetChannelSpace ─▶                    |                    |
       |   (X, exclusive=true)                  |                    |
       |                    |                    |                    |
       |       ╔════════════╧════════════════════╧════════╗          |
       |       ║  ✅ [FIX] exclusive ブロック              ║          |
       |       ║                                          ║          |
       |       ║  running_on_dll=1 >= dll_max=1           ║          |
       |       ║                                          ║          |
       |       ║  ★ 要求チャンネルが既に動作中か確認        ║          |
       |       ║  get(Channel X key) → is_running=true    ║          |
       |       ║  requested_already_running = true         ║          |
       |       ║                                          ║          |
       |       ║  → 退避をスキップ！                        ║          |
       |       ╚════════════╤════════════════════╤════════╝          |
       |                    |                    |                    |
       |                    |  同一チャンネル再利用チェック             |
       |                    |  → Channel X 発見！is_running=true      |
       |                    |── cancel_idle_close(Channel X) ─▶       |
       |                    |  stop_warm_tuner()                      |
       |                    |  current_tuner = Channel X (再利用)      |
       |◀── SetChannelSpaceAck |                  |                    |
       |    (success=true)  |                    |                    |
       |    ※即時返却・リーダー再起動なし          |                    |
       |                    |                    |                    |
       |── StartStream ─────▶                    |                    |
       |                    |── subscribe() ──────▶                    |
       |                    |  subscriber_count: 1 → 2                |
       |◀── StartStreamAck ─|                    |                    |
       |                    |                    |                    |
       |         ╔══════════╧════════════════════╧══════════════════╗ |
       |         ║      TSデータ配信（Session A と共有）              ║ |
       |◀── TS data ──────────────────────────────────────────────TS ▶|
       |         ╚═══════════════════════════════════════════════════╝ |
       |                    |                    |  ✅ Session A 継続中 |
```

---

## 4. 修正コード差分

**ファイル:** `recisdb-proxy/src/server/session.rs`

```rust
if running_on_dll >= dll_max {
+   // ★ 退避前に「要求チャンネルが既に動作中か」を確認。
+   // 動作中なら新スロット不要 — 同一チャンネル再利用パスに委ねる。
+   let req_spec = ChannelKeySpec::SpaceChannel {
+       space: actual_space, channel: actual_bon_channel
+   };
+   let requested_already_running = {
+       let mut found = false;
+       for k in keys.iter() {
+           let is_match = if !nid_tsid_channel_keys.is_empty() {
+               // グループモード: NID+TSID で照合
+               nid_tsid_channel_keys.iter().any(|(p, s)| k.tuner_path == *p && k.channel == *s)
+           } else {
+               // シングルチューナーモード: 完全一致
+               k.tuner_path == tuner_path && k.channel == req_spec
+           };
+           if is_match {
+               if let Some(t) = self.tuner_pool.get(k).await {
+                   if t.is_running() { found = true; break; }
+               }
+           }
+       }
+       found
+   };
+
+   if requested_already_running {
+       // 要求チャンネル動作中 → 退避不要、同一チャンネル再利用パスへ
+       info!("... skipping eviction, will reuse existing reader");
+   } else {
        // 既存の退避ロジック（idle → any の順で退避候補を選択）
        info!("... evicting to make room");
        let mut best_idle = None;
        let mut best_any  = None;
        // ... (変更なし)
+   }
}
```

---

## 5. セッション状態遷移

```text
                      TCP接続
                         │
                         ▼
                    ┌─────────┐
                    │ Initial │  ← Hello / HelloAck
                    └────┬────┘
                         │ HelloAck
                         ▼
                    ┌─────────┐
                    │  Ready  │
                    └────┬────┘
                         │ OpenTuner → OpenTunerAck
                         ▼
              ┌──────────────────────┐
              │      TunerOpen       │◀─────────────────────┐
              └──┬───────────────────┘                      │
                 │                  ▲                       │
                 │ SetChannel(Space) │ SetChannel(Space)     │ StopStream
                 │ ※チャンネル選択   │ ※チャンネル切替       │ → StopStreamAck
                 │                  │                       │
                 │ StartStream       │                  ┌────┴──────┐
                 │ → StartStreamAck  └──────────────────│ Streaming │
                 └──────────────────────────────────────▶└──────────┘
                         │
                         │ CloseTuner → CloseTunerAck
                         ▼
                    ┌─────────┐
                    │  Ready  │
                    └────┬────┘
                         │ TCP切断 / エラー
                         ▼
                    ┌─────────┐
                    │ Closing │ ← クリーンアップ
                    └─────────┘  (unsubscribe, idle-close スケジュール)
```

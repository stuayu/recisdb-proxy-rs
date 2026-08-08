# チューナー選択・配信・切り替え経路の再設計 (2026-08)

対象: `recisdb-proxy` のクリティカルパス — チューナー選択ロジック、TS配信機構、
チャンネル切り替え処理。

位置づけ: `docs/SYSTEM_REVIEW_2026-07.md` のリファクタリング・ロードマップ
Phase 2 項目 **17 (SetChannel ポリシーエンジン抽出)** と **18 (Session 状態機械の
型による強制)** の実施計画。本書が設計の正本であり、完了後は `DESIGN.md` §4.3〜4.5
および `STREAMING_DESIGN.md` に反映して本書はアーカイブする。

---

## 1. 現状の構造

### 1.1 選局決定の散在

同じ「どのチューナーでどのチャンネルを開くか」の判断が 4 系統に分かれ、それぞれ
異なるポリシーを持っている。

| 経路 | 入口 | 特徴 |
|---|---|---|
| BNDP v2 空間選局 | `session.rs::handle_set_channel_space` | 本流。ヘルパ 8 個に分岐 |
| BNDP v1 選局 | `session.rs::handle_set_channel` | 上と似て非なる分岐 |
| 論理チャンネル選局 | `session.rs::handle_select_logical_channel` | 同一DLL切替を常に同期停止 |
| HTTP / Mirakurun | `channel_resolve::start_tuner_for_service` | セッション経路に無い `evict_idle_on_path` / EALREADY 再試行を独自実装 |

`handle_set_channel_space` から呼ばれるヘルパ:
`try_reuse_existing_set_channel_space_tuner` /
`handle_set_channel_space_exclusive_access` /
`handle_set_channel_space_capacity_limit` /
`finish_set_channel_space_with_new_tuner` /
`try_start_set_channel_space_new_tuner` /
`finalize_set_channel_space_new_tuner` /
`try_fallback_drivers` /
`try_finish_set_channel_space_via_fallback`。

各ヘルパが独立に DB とプールを再走査するため、1 回の選局で同じ問い合わせが複数回
発生し、かつ判断の前提となるスナップショットが互いにずれる。

### 1.2 リーダー起動経路の二重化

- `SharedTuner::start_bondriver_reader` (cold start)
- `WarmTunerHandle::activate` (prewarm 済みハンドルの起動)

両者ともタイムアウト 10 秒をハードコードし、`reader_handle` の設定タイミングと
失敗時の後始末が異なる。

---

## 2. 検出した欠陥

### 2.1 安定性

1. **ready 待ちと SetChannel 再試行のタイムアウト逆転**
   `start_bondriver_reader` は `ready_rx` を 10 秒で打ち切るが
   (`tuner/shared.rs`)、blocking 側の SetChannel 再試行ループは
   `set_channel_retry_timeout_ms` (既定 10_000) まで回る。呼び出し側が先に
   タイムアウトするとプール entry は削除される一方、blocking スレッドはその後
   SetChannel に成功してリーダーループへ入る。**誰も参照しない孤児リーダーが
   DLL スロットを占有し続ける。**

2. **容量判定が全て TOCTOU**
   `count_running_instances_on_driver` は `tuner_pool.keys()` の後にキー毎
   `get()` を呼ぶ非原子スナップショット。判定から `start_bondriver_reader`
   完了 (最大 10 秒) までの間に他セッションが起動すると `max_instances` を超える。
   スロット予約の概念が存在しない。

3. **リーダー再起動が完了を待ちきらない**
   `start_bondriver_reader` は旧リーダー停止を 500ms 待って諦め、そのまま新規
   起動へ進む。同一 DLL 上で旧 blocking スレッドと新スレッドが一時共存し得る。
   `dll_init_lock` は初期化区間しか保護せず、eviction / stop 側は取得しない。

4. **warm tuner が容量計上の外にいる**
   prewarm は最大 `prewarm_timeout_secs` (既定 30 秒) DLL を open 保持するが、
   `count_running_instances_on_driver` にも `evict_idle_on_path` にも見えない。
   単一 open 制約のデバイス (px4 系 `/dev/px4videoN`) では他の要求を全て
   EALREADY で落とす。

5. **プール entry の状態判定が 7 箇所に重複**
   「`is_running` と `has_subscribers` の組み合わせで stale を判定して除去」する
   コードが `pool.get_or_create` (2 箇所) / `pool.cleanup` /
   `pool.evict_idle_on_path` / `pool.schedule_idle_close` /
   `try_reuse_existing_set_channel_space_tuner` / `try_fallback_drivers` に
   散在。レビュー指摘 M8 (生成直後チューナーの誤 evict 競合) は未解消。

6. **購読カウントが手動**
   `SharedTuner::unsubscribe` に「カウント 0 での減算を弾く」防御が入っている
   こと自体が、リーク・二重解放が実在した証跡。セッション / HTTP / encoder が
   それぞれ subscribe / unsubscribe を手で対応付けている。

7. **退避されたセッションへの通知が無い**
   排他 eviction は購読者のいるチューナーも停止するが、被害者側は 2 秒周期の
   `reader_alive_check` でようやく検知して無言切断する。

8. **eviction ポリシーの不一致**
   排他経路 (`handle_set_channel_space_exclusive_access`) は購読者ありでも
   停止する。容量経路 (`find_lowest_priority_idle_tuner`) は idle のみ対象。
   優先度比較が `>=` のため同値でも退避が起きる。

### 2.2 高速性

9. **TS チャンクの多重コピー**
   reader の `Bytes::copy_from_slice` → session の `ts_send_carry` (Vec extend)
   → 送出時の `Bytes::copy_from_slice` → フレーム組み立ての `BytesMut::put_slice`
   で 3〜4 回。購読者数に比例して増える。

10. **reader の blocking スレッドが毎チャンク PSI 解析**
    `logo_collector.process_ts_chunk` と `epg_collector.process_ts_chunk` が
    読み取り律速点で同期実行される。

11. **TS 品質解析がセッション毎に重複**
    同一チューナーの N 購読者が同じバイト列を N 回解析する。

12. **encoder 使用中も raw ブロードキャストを受信して破棄**
    `session.rs` の TS 受信分岐で `current_encoder.is_some()` のとき
    `let _ = data;` としているだけで、broadcast のコピーは発生済み。

13. **空読み時のスリープが多重**
    `wait_ts_stream(100)` の上にさらに `thread::sleep(10ms)` を重ね、起動時は
    `consecutive_empty` を 40000 回まで許容する。`wait_ts_stream == false` /
    `n == 0` / `WouldBlock` の 3 状態が同じカウンタに混在している。

14. **DB / プールへの過剰アクセス**
    `collect_group_channel_candidates` が 1 回の選局で 2 回実行される。
    `find_lowest_priority_idle_tuner` はループ内で `database.lock()` を取る。
    候補ドライバ毎に `keys()` + `get()` を再走査する。

---

## 3. 目標とする構造

```
             ┌──────────────── 純粋 (テスト可能) ───────────────┐
TuneRequest ─┤ tuner/policy.rs :: decide(TunerSnapshot, req)   ├─→ Decision
             └────────────────────────────────────────────────┘
                                   │
                                   ▼
             ┌────────── 副作用 (唯一の実行者) ────────────────┐
             │ tuner/acquire.rs :: execute(Decision) → Outcome │
             │   予約取得 / evict / reader 起動 / 購読ハンドル  │
             └─────────────────────────────────────────────────┘
                    ▲            ▲              ▲            ▲
        SetChannel(v1)  SetChannelSpace  SelectLogical   HTTP/Mirakurun
```

- **決定は純関数**。プールと DB の状態を 1 回だけスナップショットし、以降の判断は
  I/O を伴わない。ユニットテストで挙動を固定する。
- **副作用は executor 1 箇所**。4 経路は「要求の組み立て」と「成功後のメタデータ
  適用」だけを持つ。
- **容量は数えるのではなく取る**。per-DLL のスロット予約を導入する。

---

## 4. 実施フェーズ

### P0 — 決定ロジックの純関数化

新規 `recisdb-proxy/src/tuner/policy.rs`。

```rust
pub struct DriverState {
    pub dll_path: String,
    pub max_instances: i32,
    pub quality_score: f64,
    pub exclusive_channel_count: i64,
}

pub struct EntryState {
    pub key: ChannelKey,
    pub reader: ReaderState,       // P1 で導入する状態 (P0 では bool 相当のミラー)
    pub subscribers: u32,
    pub priority: i32,
    pub reserved: bool,            // warm / 起動中を含む占有
}

pub struct TunerSnapshot {
    pub drivers: Vec<DriverState>,
    pub entries: Vec<EntryState>,
}

pub struct TuneRequest {
    pub candidates: Vec<CandidateChannel>, // (dll_path, space, bon_channel) を優先順で
    pub priority: i32,
    pub exclusive: bool,
    pub own_key: Option<ChannelKey>,       // 自セッションが手放す予定のキー
}

pub enum Decision {
    Reuse { key: ChannelKey },
    Create { key: ChannelKey, evict: Vec<ChannelKey> },
    Reject { reason: RejectReason },
}
```

- 既存の挙動 (`sort_candidate_drivers` の並び、`select_running_driver` 優先、
  容量不足時の優先度比較、fallback 順) をそのまま移植する。**この時点で挙動を
  変えない。**
- 現行の `session_driver_selection.rs` / `session_capacity.rs` の純関数群は
  policy.rs へ吸収する。
- 配線はまだ行わない。既存コードは動いたまま。
- 検証: `cargo test -p recisdb-proxy`。新規テストで、上記 §2.1-8 の不一致
  (排他経路と容量経路の evict 対象差) を「現状どうなっているか」として先に
  テストで固定してから、P1/P2 で意図的に一本化する。

### P1a — 状態モデル・購読 RAII・テスト可能化 (実装済み)

- `SharedTuner::is_running: AtomicBool` を `ReaderState` に置換した。

  ```rust
  pub enum ReaderState { Idle, Reserved, Starting, Running, Stopping, Stopped }
  ```

  `Reserved` / `Starting` を分けたことで「生成直後・起動中の entry は evict /
  stale 判定の対象外」を状態として表現でき、§2.1-5 の 7 重複と M8 の競合が
  消えた。両者を分ける理由は、答えるべき問いが違うため:

  - `Reserved` … `TunerPool::get_or_create` が entry を作ったが**リーダー起動
    はまだ誰も始めていない**。スロットは押さえているが、呼び出し元が起動する
    義務を負っている。
  - `Starting` … リーダー起動が**実行中**(BonDriver オープン〜SetChannel
    再試行)。二重起動してはならない。

  この 2 つを 1 状態にまとめると、「リーダーを起動すべきか」の判定が必ずどちらかで
  誤る(起動をサボるか、他タスクの起動に重ねて二重オープンするか)。

  導出述語:

  | 述語 | 真になる状態 | 用途 |
  |---|---|---|
  | `occupies_slot()` | Reserved / Starting / Running / Stopping | 容量計上・prewarm 抑止 |
  | `needs_reader_start()` | Starting / Running **以外** | リーダー起動の要否 |
  | `is_reclaimable()` | (Idle / Stopped) かつ購読者なし | **プール内部**の stale 掃除 |
  | `is_orphanable()` | Starting / Running 以外 かつ購読者なし | **entry の所有者**による返却 |

  旧 `is_running` はリーダー本体の先頭で `true` にしていたため**初期化中も真**
  だった。新 `is_running()` は `Running` のみなので、容量計上・二重起動防止・
  prewarm 抑止の呼び出し元は `occupies_slot()` / `needs_reader_start()` へ
  移行済み(統計表示・データ供給可否の判定だけが `is_running()` のまま)。

  `Reserved` のまま放置された entry はスロットを恒久占有するため、`get_or_create`
  の後に起動へ進まず抜ける経路(容量衝突・起動失敗・HTTP の `Busy` 返却)は
  `is_orphanable()` を見て自分でプールから除去する。**この手動の予約管理は P1b の
  スロット予約(RAII permit)で置き換える。**

- **購読の RAII 化**: `TunerSubscription { tuner: Arc<SharedTuner>, rx }` を
  導入し `Drop` で減算する。`subscribe` / `unsubscribe` の手動対応付けと
  wraparound 防御を撤去する。encoder の `subscribe_untracked` は
  「カウントしない購読」であることを型で表す (`UntrackedSubscription`)。

- **スロット予約**: `TunerPool` に `DriverSlots` (dll_path → `Semaphore`、
  permit 数 = `max_instances`) を追加。reader 起動と warm tuner の双方が
  permit を取得し、`SharedTuner` / `WarmTunerHandle` の生存期間だけ保持する。
  これにより §2.1-2 の TOCTOU と §2.1-4 の warm 計上漏れが同時に解消する。
  `max_instances` の変更時は permit 数を増減させる (`add_permits` / forget)。

- **テスト可能化**: reader をトレイト (`TsReader`) で抽象化し、テスト用の
  フェイク実装を注入できるようにする。現状 `is_running` は実 reader からしか
  遷移せず、プールの競合系がテスト不能になっている
  (`pool.rs` のテストコメント参照)。

- 検証: `cargo test -p recisdb-proxy`。フェイク reader を使った競合テスト
  (生成中 evict / idle-close と subscribe の競合 / 容量超過の同時要求) を追加。

### P2 — 選局経路の一本化

- 新規 `recisdb-proxy/src/tuner/acquire.rs` に唯一の executor を置く。
  4 経路すべてがここを通る。`channel_resolve::start_tuner_for_service` も
  薄いラッパにする。
- `session.rs` から選局系ヘルパ 8 個を削除。`Session` は要求の組み立てと
  `apply_channel_metadata` 相当の後処理のみ持つ。
- **DLL init ロックの適用範囲を拡大**: 取得を `start_bondriver_reader` 内部へ
  移し、stop / evict も同じロックを取る (§2.1-3)。
- **リーダー起動 API の一本化**: cold / warm を単一の
  `start_reader(target, warm: Option<WarmTunerHandle>)` に統合し、失敗時の
  後始末を 1 箇所にする。
- **タイムアウトの整合**: ready 待ちを
  `set_channel_retry_timeout_ms + マージン` から算出する。加えて blocking 側は
  ready 送信に失敗した (= 受信側が消えた) 場合、リーダーループへ入らず即座に
  終了する (§2.1-1)。
- ハードコード時定数 (安定化 sleep 500ms / stop タイムアウト 1s /
  EALREADY 再試行 300ms / `wait_first_data` の 50ms ポーリング) を
  `TunerPoolConfig` に集約し、`recisdb-proxy.toml.example` に追記する。
- **eviction ポリシーの統一** (§2.1-8): 「idle を優先、購読者ありの退避は
  `exclusive` かつ要求優先度が厳密に上回る場合のみ」に一本化し、優先度比較を
  `>` にする。
- 検証: `cargo test -p recisdb-proxy` + 実機での連続チャンネル切り替え。

### P3 — 配信経路の高速化

- reader の出力を rolling `BytesMut` + `split_to().freeze()` にしてコピーを
  1 回に減らす (§2.2-9)。session 側のフレーム組み立てはヘッダとペイロードの
  vectored write にする。
- logo / EPG 収集を reader スレッドから専用タスクへ退避 (§2.2-10)。
- TS 品質解析をチューナー単位 1 回に集約し、セッションは結果を購読する
  (§2.2-11)。
- encoder 使用中のセッションは raw 購読を解除し、キープアライブ用のトークンのみ
  保持する (§2.2-12)。
- 空読み時のスリープ・カウンタ体系を整理する (§2.2-13)。
- 検証: 実機でのビットレート / CPU 使用率の前後比較。

### P4 — 退避通知と可観測性

- evict される購読者に明示メッセージを送出し、2 秒ポーリング検出
  (`reader_alive_check`) を撤去する (§2.1-7)。
- 決定 1 件 = 1 行の理由付きログ (`decision=create key=... reason=...`) と、
  直近の decision trace を `/api` に露出する。

### P5 — ドキュメント反映

- `DESIGN.md` §4.3 (チューナー共有) / §4.4 (優先度・排他・容量制御) /
  §4.5 (グループ選局) を新モデルへ改訂。
- `STREAMING_DESIGN.md` のデータパス図を P3 後の形へ更新。
- `SYSTEM_REVIEW_2026-07.md` の Phase 2 項目 17 / 18 を完了扱いに更新。
- `CLAUDE.md` の不変条件に以下を追加:
  「選局の決定は `tuner/policy.rs` の純関数、副作用は `tuner/acquire.rs` の
  executor のみ。新しい選局経路を session / web に直接書かない」。
- `recisdb-proxy.toml.example` に P2 で追加した設定項目を反映。

---

## 5. 影響範囲と非影響範囲

- **チャンネル列挙順は変更しない**。`server/client_view.rs` と channels テーブルの
  内容に触れないため、`.ch2` / ChSet の再生成は不要。
- DB スキーマ変更なし (`MIGRATIONS` への追記は発生しない)。
- BNDP プロトコルは P4 の退避通知メッセージ追加のみ。それまでは互換。
- `bondriver-proxy-client` 側は P4 まで変更なし。

## 6. リスク

- 実機 BonDriver が無い環境では結合検証ができない。P1 で導入するフェイク reader
  により、プール・ポリシー層は CI で検証可能になるが、DLL 実挙動 (SetChannel の
  遅延、EALREADY、GetTsStream の切り詰め) は実機確認が必要。
- P2 は 3 経路の挙動差を意図的に潰すため、既存クライアントの体感が変わる箇所が
  ある (特に排他選局時の退避対象)。P0 で現状挙動をテストに固定してから変更する。

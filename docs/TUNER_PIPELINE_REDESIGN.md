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

### P1b — スロット予約 (実装済み)

`TunerPool` に `DriverSlots` (dll_path → `Semaphore`、permit 数 =
`max_instances`) を追加し、容量を**数えるのではなく取る**方式へ変えた。
これにより §2.1-2 の TOCTOU と §2.1-4 の warm 計上漏れが解消した。

- `TunerPool::acquire_slot(dll_path, max_instances) -> Option<SlotPermit>`。
  プールは DB を持たないので `max_instances` は引数で受け取り、値が変わって
  いれば `add_permits` / `forget_permits` で追随する。**満杯なら待たずに
  `None`** を返し、退避・フォールバック・失敗の判断は呼び出し元に委ねる。
- `SlotPermit` は `Drop` で解放される。`SharedTuner` が保持し、
  `stop_reader()` とリーダーの全異常終了経路 (`stop_and_release_slot`) で
  明示的に解放する (Arc の drop 待ちにしない)。
- **`start_bondriver_reader` / `WarmTunerHandle::activate` の引数に
  `SlotPermit` を必須化**した。permit なしにリーダーを起動できないことを
  型で保証している。
- **permit 移譲**: 同一セッションが同じ DLL 上でチャンネルを切り替えるとき、
  旧チューナーから permit を取り出して新しいエントリへ直接渡す。旧リーダーは
  候補が成功するまで停止されないため、`max_instances=1` では「解放してから
  取得」も「取得してから解放」も成立しない。これが `old_tuner_will_free_slot`
  (自セッション分を容量計数から除外する仕掛け) の置き換えであり、同フラグは
  撤去した。`SelectLogicalChannel` の候補ループでは、候補が失敗したときに
  permit を回収して次の候補へ持ち回り、全候補が失敗したら旧チューナーへ
  返却する (旧リーダーは動き続けるため)。
- **再利用は permit 取得より先**に判定する。順序を誤ると、`max_instances=1`
  のドライバーで既存チャンネルへ合流できるはずのリクエストが容量不足として
  弾かれる。
- `count_running_instances_on_driver` などの計数は**診断・選択ヒューリスティック
  専用**に降格した。容量の強制は `DriverSlots` のみが行う。
- P1a の `Reserved` 状態と `is_orphanable()` による手動返却は、permit の
  `Drop` があるため保険の位置づけになった。

- **テスト可能化**: reader をトレイト (`TsReader`) で抽象化し、テスト用の
  フェイク実装を注入できるようにする。現状 `is_running` は実 reader からしか
  遷移せず、プールの競合系がテスト不能になっている
  (`pool.rs` のテストコメント参照)。

- 検証: `cargo test -p recisdb-proxy`。フェイク reader を使った競合テスト
  (生成中 evict / idle-close と subscribe の競合 / 容量超過の同時要求) を追加。

### P2a — リーダー起動経路の一本化 (実装済み)

P2 のうち「リーダー起動経路」のみを対象にした最初の区切り。選局ポリシー
(退避・優先度・排他・フォールバック) と `tuner/acquire.rs` の抽出は
P2b に残した (下記)。

- **孤児リーダーの根絶 (§2.1-1)**: 両側から塞いだ。
  - 待ち側: ready 待ちのタイムアウトを固定 10 秒ではなく
    `tuner/timing.rs::reader_ready_timeout(set_channel_retry_timeout_ms)`
    (`= set_channel_retry_timeout_ms + READY_TIMEOUT_MARGIN_MS(5000)`) から
    算出するようにした。SetChannel 再試行の予算より必ず長くなる。cold open
    (`SharedTuner::start_reader_cold`) と warm 活性化
    (`WarmTunerHandle::activate`) の両方がこの一つの関数を使う。
  - リーダー側: `run_bondriver_reader_with_tuner` の成功パスで
    `ready_tx.send(Ok(()))` の**戻り値を見る**ようにした。送信に失敗した
    (= 受信側がタイムアウトして消えた) 場合は読み取りループへ入らず
    その場で `stop_and_release_slot()` して終了する。失敗パスは元々
    `stop_and_release_slot()` を送信より先に呼んでいたため対称になっていた
    が、成功パスだけこの確認が抜けていたのが本丸だった。
- **リーダー起動 API の一本化**: `SharedTuner::start_reader(tuner_pool,
  tuner_path, space, channel, startup_config, permit, warm: Option<WarmTunerHandle>)
  -> io::Result<()>` を新設し、cold/warm の分岐をこの関数の内部
  (`start_reader_cold` / `start_reader_warm` という非公開メソッド) に閉じ込めた。
  旧 `SharedTuner::start_bondriver_reader` (pub) と `WarmTunerHandle::activate` (pub)
  は前者は削除、後者は `pub(crate)` に降格し、どちらも `start_reader` からのみ
  呼ばれる。`session.rs::start_reader_with_warm` はセッション固有の
  warm ハンドル持ち回り (`self.warm_tuner`/`warm_tuner_path`) だけを残した
  薄いラッパになった。`channel_resolve::start_tuner_for_service` は
  `warm: None` で同じ `start_reader` を呼ぶ (HTTP 経路には warm ハンドルが
  存在しないため)。
  - 失敗時の後始末を 1 箇所に集約: `activate()` の途中失敗
    (`ensure_ready()` 失敗 / コマンド送信失敗 / 応答チャネルが理由不明に
    閉じた) は、それまで `SharedTuner::stop_and_release_slot()` を呼ばずに
    permit を握ったままにしていたリークがあったので、`activate()` 自身が
    releaseするよう修正した (`ready` 待ちタイムアウトの一系統だけは、まだ
    SetChannel 中かもしれないリーダー側の送信失敗に委ねるため意図的に
    release しない — cold open と同じ理屈)。
  - warm 起動スレッド (`WarmTunerHandle::spawn`) のトップレベル
    `catch_unwind` も、`WarmCommand::Start` 実行中の panic では permit を
    解放していなかった (cold open 側の同等ハンドラは元から解放していた)。
    `started_shared` に実行対象を控えておき、panic 時に解放するよう揃えた。
  - **warm 失敗時の cold フォールバックは維持する。** 旧 `session.rs` は
    warm 活性化に失敗すると permit を回収して cold open へ切り替えていた。
    これを落とすと、`prewarm_timeout_secs` (既定 30 秒) が切れて warm
    スレッドが終了した後の選局 — クライアントがチャンネル一覧を眺めてから
    選局するという日常的な操作 — が、cold で開けば済むのに失敗する。
    フォールバックの可否は「warm スレッドがもう DLL を掴んでいないか」で
    決まるため、`WarmTunerHandle::activate` は次のように区別する:
    - `ErrorKind::NotConnected` … warm スレッドは既に終了 (open 失敗、または
      コマンド待ちタイムアウトで終了)。DLL は解放済みなので、**permit を
      `SharedTuner` に残したまま**返す。`start_reader_warm` はこのエラー種別
      のときだけ、同じスロットで cold open に切り替える。
    - ready 待ちタイムアウト … warm スレッドがまだ SetChannel 中で DLL を
      掴んでいる可能性があるため、cold で開くと二重オープンになる。permit は
      残すが (リーダー側が自分で解放する)、フォールバックはしない。
    - それ以外 … 既に `run_bondriver_reader_with_tuner` が走っており、
      その内部の失敗経路が permit を解放済み。
- **DLL init ロックの内部化**: `tuner_pool.acquire_dll_init_lock()` の取得を
  `start_reader` の内部に移し、`session.rs`/`channel_resolve.rs` の呼び出し元
  からは撤去した (取り忘れが構造的に起きなくなった)。`stop_reader()` 側は
  引き続きこのロックを取らない — P1b のスロット permit により旧リーダーが
  `Stopped` になるまで permit が解放されないため、新しいオープンが旧ハンドルと
  重なることはもう起きないという判断はそのまま踏襲した。
- **再起動待ちの是正 (§2.1-3)**: `start_reader` の冒頭、`self` に既存の
  reader (Starting/Running/Stopping) がある場合は新設の
  `stop_existing_reader_before_restart()` で `stop_reader()` を確実に待ち、
  戻り値 (`bool`) が `false` (タイムアウト) ならエラーを返して新規オープンに
  進まない。旧実装の「500ms 待って諦めて続行」は撤去した。`stop_reader()` 自体
  も `bool` を返すよう変更 (`true`=確認できた停止、`false`=タイムアウト) —
  既存の呼び出し元は戻り値を無視するだけで済むため後方互換。
- **時定数の集約 (`tuner/timing.rs`)**:

  | 定数 | 値 | 根拠 |
  |---|---|---|
  | `SET_CHANNEL_STABILIZATION_SLEEP_MS` | 500ms | SetChannel 直後、新規ドライバのバッファに何か溜まるまでの安定化待ち |
  | `STOP_READER_TIMEOUT_MS` | 1000ms | `stop_reader()` の handle ロック取得・join の各タイムアウト。読み取りループの stop チェック間隔 (100ms) の約5倍のマージン |
  | `WAIT_TS_STREAM_POLL_MS` | 100ms | 読み取りループの `wait_ts_stream` ポーリング間隔 |
  | `WAIT_FIRST_DATA_POLL_MS` | 50ms | `wait_first_data` のポーリング間隔 |
  | `EALREADY_RETRY_SLEEP_MS` | 300ms | `channel_resolve` の EALREADY 再試行前、evict した idle リーダーの fd が実際に閉じるまでの猶予 |
  | `READY_TIMEOUT_MARGIN_MS` | 5000ms | ready 待ちタイムアウト = `set_channel_retry_timeout_ms + この値`。BonDriver オープン自体 (再試行ループの外側) の所要時間とスケジューリング余裕を見込む |

  **DB (`tuner_config` テーブル) には追加しなかった**: これらはユーザーが
  デバイス/ネットワークの事情に応じて調整する `set_channel_retry_timeout_ms`
  等とは性質が異なり、(a) 読み取りループ自身のポーリング間隔から導出される
  内部マージン、または (b) プロトコル上決め打ちでよい固定値であり、
  「触る理由がある値」ではない。DB 化すると `TunerPoolConfig` へのフィールド
  追加 → マイグレーション → ダッシュボード改修まで波及するが、それに見合う
  運用上の必要性がない。

- 追加したテスト (`tuner/shared.rs`, `tuner/timing.rs`, `tuner/ts_source.rs`):
  - `tuner::timing::tests::reader_ready_timeout_*`: ready タイムアウトが
    常に `set_channel_retry_timeout_ms` を上回ることの単体テスト。
  - `tuner::shared::tests::ready_send_failure_after_success_releases_slot_without_entering_read_loop`:
    §2.1-1 の核心 — 呼び出し元が ready を待たずに消えた場合、読み取り
    ループへ入らず permit が解放されることを確認 (`FakeTsSource` + 実際の
    `TunerPool::acquire_slot`)。
  - `tuner::shared::tests::stop_existing_reader_before_restart_waits_for_a_healthy_reader` /
    `..._errors_if_reader_does_not_stop_in_time`: §2.1-3 —
    健全なリーダーは確実に停止してから restart 扱いになること、
    詰まったリーダー (`FakeTsSource::with_get_ts_stream_gate` で
    `stop_reader()` のタイムアウトを決定的に発火させる) はエラーになる
    ことを確認。
  - 既存の P1a/P1b テストは全て green (計 338 件、macOS 既知の setup_gui
    1 件のみ ignore)。
  - **テスト不能な範囲**: `start_reader_cold` の `BonDriverTuner::new(...)`
    (実 FFI) 経路と `WarmTunerHandle::spawn` の同経路は実機 DLL が無いと
    到達できない。cold/warm 起動 API 自体の「open 失敗時に Stopped +
    permit 解放」は、両者が最終的に共有する
    `run_bondriver_reader_with_tuner`/`stop_and_release_slot` のレベルでは
    既存テストが検証しているが、`BonDriverTuner::new` 自体の失敗パスは
    実機確認が必要。手動検証: 実在しない DLL パスを指定してチャンネル
    切り替えを行い、(1) エラーが返る、(2) 同じ DLL パスへの後続リクエストが
    ブロックされず成功する (permit が残っていないこと) を確認する。

### P2b-1 — executor の導入と HTTP 経路の配線 (実装済み)

副作用を実行する唯一の場所として新規 `recisdb-proxy/src/tuner/acquire.rs` を追加し、
まず HTTP/Mirakurun 経路 (`channel_resolve::start_tuner_for_service`) だけをそこへ
載せ替えた。セッション経路 (`session.rs`) の配線は P2b-2、eviction ポリシーの
一本化は P2b-3 に残る。

- **`snapshot()`**: `tuner::policy::TunerSnapshot` を実プール + DB から 1 回だけ
  組み立てる。`tuner_pool.keys()`/`tuner_pool.get()` の await をすべて終えてから
  `database.lock()` を 1 回だけ取り、そのガードのスコープ内は `rusqlite` の
  同期呼び出し (`get_max_instances_for_path` / `get_driver_quality_score_by_path` /
  `get_exclusive_channel_counts` / 各 entry の `get_channel_priority`) だけで
  await を一切挟まない構造にした。これは SYSTEM_REVIEW_2026-07.md H3
  (「DB ロックを保持したまま tuner_pool を await してはならない」) を
  レビューではなく構造で保証するための順序で、`snapshot()` の doc comment に
  明記している。`dll_paths` 引数は候補に登場するドライバのみに絞り込む
  ―― 関係ない entry/driver 行は最初から取得しない。

- **`acquire()` の API**:

  ```rust
  pub(crate) struct AcquireRequest {
      pub candidates: Vec<ChannelKey>,
      pub priority: i32,
      pub exclusive: bool,
      pub bondriver_version: u8,
      pub carried_permit: Option<SlotPermit>,
      pub warm: Option<WarmTunerHandle>,
  }

  pub(crate) struct AcquireOutcome {
      pub tuner: Arc<SharedTuner>,
      pub key: ChannelKey,
      pub reused: bool,
      pub unused_permit: Option<SlotPermit>,
      pub unused_warm: Option<WarmTunerHandle>,
  }

  pub(crate) async fn acquire(
      pool: &Arc<TunerPool>,
      database: &DatabaseHandle,
      request: AcquireRequest,
  ) -> Result<AcquireOutcome, AcquireError>;
  ```

  `AcquireRequest` は意図的に `own_key`/`own_key_will_free_slot`
  (`policy::TuneRequest` が持つ、同一セッションの手放し予定キー) を持たない ――
  P2b-1 で配線した HTTP 経路はそもそも自分の tuner を保持したまま次を要求する
  ことがないため。P2b-2 で session.rs を配線する際に追加される想定。

  `AcquireError` (`thiserror`) は `NoCandidates` / `AtCapacity { lowest_idle_priority }`
  (`policy::RejectReason` をそのまま写す) / `Conflict(u32)` (下記の再試行上限
  超過) / `ReaderStart(#[from] io::Error)` / `Pool(#[from] TunerPoolError)`
  の 5 種類を区別する。

- **`acquire()` の処理**: 候補の DLL パスで `snapshot()` → `policy::decide()` →
  `Decision` の実行、の 1 ラウンド。
  - `Reuse { key }`: `pool.get(&key)` で取得して返すだけ ―― permit は一切
    取得しない。P1b §6 の「再利用は permit 取得より先」が構造的に保証される
    (このブランチにコードとして `acquire_slot` 呼び出しが存在しない)。
  - `Create { key, evict }`: まず `evict` を停止・除去する
    (`SharedTuner::stop_reader()` が戻り値を返す時点で permit を確定的に
    解放しているため、`server::session_capacity::stop_and_remove_tuner` が
    P1b 以前に必要としていた「解放待ちポーリング」は不要 ―― `acquire.rs`
    内に `stop_and_remove_tuner` 相当の `evict_tuner()` を独自実装した。
    `session_capacity` 側の関数は `pub(super)` で `server` モジュール限定の
    ため、そのまま import はできない)。次に permit を
    `carried_permit`(パス一致時) → `warm` ハンドルの permit(パス一致時) →
    `pool.acquire_slot` の優先順で取得し (`take_permit_for_path` ヘルパ。
    `carried_permit` が勝った場合、同じパスの `warm` は用済みとして
    shutdown する ―― 両方を要求時に活性化する経路は無いため)、
    `pool.get_or_create` → `tuner.take_slot_permit()` →
    `tuner.start_reader(...)` と進む。起動失敗時は `is_orphanable()` を
    見て entry を後始末する。
  - `Reject { reason }`: 対応する `AcquireError` に変換して返す。

- **競合検知時の再試行**: 1 ラウンドの途中で以下のいずれかを検知したら、
  新しい `snapshot()` からやり直す (`MAX_ACQUIRE_ATTEMPTS = 3` 回まで、
  超えたら `AcquireError::Conflict(3)`):
  1. `Decision::Reuse` が指した entry が `pool.get()` 時点で消えていた
     (concurrent stop/evict と競合)。
  2. `Decision::Create` の永続的な permit 取得元 (`carried_permit`/`warm`/
     `pool.acquire_slot`) がすべて失敗した (snapshot が「空きがある」と
     見たのに実際のセマフォには無かった)。
  3. `pool.get_or_create` が `Decision::Create` にもかかわらず
     `needs_reader_start() == false` な entry (他タスクが同じキーを
     先に作って起動済み/起動中) を返した。
  これは `policy.rs` の doc comment にある「executor は競合を検知したら
  decide を呼び直す」の実装であり、`decide` 自身は複数ラウンドの I/O を
  シミュレートしない (モジュール doc comment参照)。

- **HTTP 経路 (`channel_resolve::start_tuner_for_service`) が `acquire()` に
  委ねた処理と、残した処理**:
  - 委ねた: 容量判定そのもの (permit 取得の成否)、`decide()` による
    idle 優先度ベースの eviction、reader 起動、失敗時の entry 後始末。
  - 残した: Unix の単一 open 制約 (px4 系デバイス) 対策である
    `evict_idle_on_path` の proactive (容量到達時に `acquire()` を呼ぶ前に
    先回りで idle reader を退避) と reactive (`AcquireError::ReaderStart`
    が `EALREADY` のときに退避して 1 回だけ `acquire()` をやり直す) の
    両方 ―― これは `max_instances` とは独立な物理デバイス制約で、
    `decide()` のポリシーが知る話ではない。
  - `AcquireError` → `ChannelResolveError` の変換 (`map_acquire_error`):
    `ReaderStart`/`Pool` はそのまま同名 variant へ、`AtCapacity`/`Conflict`
    は既存の `Busy { id, running, max }` へ (`running`/`max` の診断値は
    `acquire()` が知る必要のない HTTP 固有の情報なのでここで計算する)。
  - `AcquireRequest` の組み立て: 候補は `resolved.channel_key` の 1 つのみ
    (フォールバック探索なし)、`exclusive: false` (HTTP 視聴要求は他セッションの
    ライブ購読者と競合しない、という既存の保証を「`exclusive` 分岐が
    到達しない」という構造で維持)、`carried_permit`/`warm` は常に `None`
    (HTTP 経路はどちらも保持したことがない)。
  - `priority` には `resolved.channel.priority` (このチャンネル自身の DB
    優先度) を採用する。`session.rs` の `SetChannelSpace` がクライアント
    優先度未指定時に使う「DB デフォルト」分岐と同じ値であり、退避可否の
    比較が「DB 優先度どうし」で対称になる。

    HTTP 経路は P2b-1 以前、`decide()` の優先度付き eviction を通っておらず
    優先度という概念を持っていなかったため、これは経路の挙動追加にあたる。
    ただし**旧挙動は完全に包含されており、退行はない**:
    - パス単位の無条件 idle 退避 (`evict_idle_on_path`) は `acquire()` の
      **前**に従来どおり実行される。Unix 単一 open 制約への対処なので、
      優先度で条件付けてはならない。
    - その後に走る `decide()` の容量制限 eviction は、旧経路が
      `Busy` を返して諦めていた場面に**追加の**退避機会を与えるだけ。
    - `exclusive: false` なので、購読者のいるリーダーを奪う分岐
      (`decide_exclusive_at_capacity`) には到達しない。「HTTP リクエストが
      他セッションのライブ視聴を止めることはない」という既存の保証は
      構造で維持されている。

    P2b-3 で優先度比較を `>=` から `>` に変える際、この経路も同じ規則に
    従う (同値では退避しない)。

### P2b-2 — SetChannelSpace を acquire へ載せ替え (実装済み)

BNDP v2 空間選局 (`handle_set_channel_space`) を `acquire()` に載せ替え、
そこにぶら下がっていた選局ヘルパ 8 個を削除した。

削除したヘルパと責務の移動先:

| 削除したヘルパ | 移動先 |
|---|---|
| `try_reuse_existing_set_channel_space_tuner` | `decide()` の `Reuse` 分岐 |
| `handle_set_channel_space_exclusive_access` | `decide()` の排他分岐 |
| `handle_set_channel_space_capacity_limit` | `decide()` の容量分岐 + `acquire` の permit 取得 |
| `try_fallback_drivers` | `acquire` の候補リスト (`decide` が再ソートして選ぶ) |
| `try_finish_set_channel_space_via_fallback` | 同上 |
| `finish_set_channel_space_with_new_tuner` | `acquire` |
| `try_start_set_channel_space_new_tuner` | `acquire` (起動前の再競合チェックは再試行ループへ) |
| `finalize_set_channel_space_new_tuner` | `acquire` (起動後の排他再チェックは再試行ループへ) |
| `finish_set_channel_space_fallback_success` | `finish_set_channel_space_success` に統合 |

`session.rs` は 3613 行 → 3210 行。`session_capacity.rs` からも
`find_lowest_priority_idle_tuner` / `ensure_driver_capacity_with_idle_eviction` /
`evict_interlopers_until_capacity` が不要になり削除した。

セッションが組み立てるもの:

- **候補リスト**: 選択したドライバを先頭に、同じ (NID, TSID) を持つグループ
  兄弟ドライバを続ける。これが旧 `fallback_candidates` チェーンの置き換え。
  `collect_group_channel_candidates` は**1 回だけ**呼ばれるようになった
  (従来は選局 1 回につき 2 回。§2.2-14)。
- **`carried_permit`**: P1b の permit 移譲。`own_key` /
  `own_key_will_free_slot` と対で `AcquireRequest` に渡す。
- **`warm`**: セッションの warm ハンドル。`AcquireOutcome::unused_warm` で
  返ってきたらフィールドへ戻す。

順序について 2 点、意図的に維持したもの:

- **旧チューナーの停止は acquire の前** ―― ただし permit を移譲する場合
  (同一 DLL かつ自セッションが唯一の購読者) に限る。単一 open のキャラクタ
  デバイス (px4 系) では、旧リーダーが握ったままの DLL を新リーダーが開こうと
  すると EALREADY になるため。別 DLL への切り替えでは従来どおり切り替え後に
  後始末する (失敗時に旧チャンネルへ戻れる)。
- **合流する場合は旧チューナーに触れない** ―― `acquire` が `Reuse` を返す
  ケース (候補の中に自分が今使っているチューナーが含まれ、かつ稼働中) では
  上記の事前停止をスキップする。旧実装で再利用チェックが後始末より前に
  あったのと同じ理由で、これをやらないと「これから合流するリーダー」を
  自分で止めてしまう。

`AcquireOutcome::unused_permit` は、旧チューナーがまだ同一 DLL 上で稼働中なら
そこへ戻す (`return_unused_permit`)。単に drop すると、デバイスが開いたままなのに
プールがスロットを空きと見なす。

### P2b-2 (続き) — v1 選局・論理チャンネル選局の配線 (実装済み)

`handle_set_channel` (BNDP v1) と `try_select_logical_channel_candidate`
(論理チャンネル選局) も `acquire()` に載せ替えた。これで**選局 4 経路すべてが
`acquire()` を通る**。

- リーダー起動が `acquire` の中だけになったため、`session.rs` の
  `start_reader_with_warm` / `acquire_slot_preferring_warm` /
  `remove_orphaned_tuner_if_unused` が不要になり削除した。warm ハンドルの
  permit を優先して使う仕組みは `acquire::take_permit_for_path`
  (carried → warm → `acquire_slot`) が担う。
- `session.rs` は 3613 行 → 3028 行 (合計 -585 行)。

**論理チャンネル選局の候補ループは残した。** この経路の候補は
`get_channels_by_nid_tsid_ordered` が返す DB 優先度順で、その順序自体に
意味がある。一方 `decide()` は候補を「排他チャンネル数 → 稼働数 → 品質
スコア」で再ソートするため、候補リストをまとめて渡すと順序の意味が変わる。
外側のループを保ち、1 候補ずつ `acquire()` を呼ぶ形にした。permit は
`&mut Option<SlotPermit>` として候補間で持ち回り、消費されなければ次の候補へ、
全候補が失敗したら旧チューナーへ返す。順序規則の統一は P2b-3 の判断事項。

**挙動を保つために `exclusive: false` を渡している経路が 2 つある**:
- v1 選局 … 旧実装は購読者のいるリーダーを退避したことがなく、容量不足なら
  単に CONFLICT を返していた。
- 論理チャンネル選局 … 同様に、退避せず次の候補へ移っていた。

どちらも `decide()` の非排他分岐しか通らないため、退避対象は idle のみに
限られる。v2 と揃えるかどうかは P2b-3 で判断する。

### P2b-3 — eviction ポリシーの一本化 (実装済み)

排他経路と容量経路で食い違っていた退避規則 (§2.1-8) を 1 本にした。
**ここは意図的な挙動変更**であり、P0 で `*_current_behavior_fixed_for_p2` と
名付けて固定していたテストは新しい期待値へ書き換えた。

新しい規則 (`policy::decide_at_capacity`):

1. このドライバの **idle (購読者なし)** リーダー。ただし退避可否は
   `may_evict` を満たすこと。
2. idle が居なければ、**購読者のいるリーダーでも**最も優先度の低いものを
   停止する。同じく `may_evict` を満たすことが条件。
3. どれも退避できなければ**別の候補ドライバ**を試す (`decide_fallback`。
   ここも同じ規則を使う)。
4. それでも駄目なら `Reject`。

`may_evict(req, victim_priority)`:

```text
req.priority > victim_priority || req.exclusive
```

- **同値では奪わない** (`>=` → `>`)。同順位の要求が先着を蹴散らしても得るものが
  無く、動いているストリームを切るだけだったため。
- **`exclusive` は同値でも奪う**。ハードウェアそのものを要求しているという
  意味なので、タイは新しい要求が勝つ。

**`max_instances` を超えて作ることは無くなった。** 従来、容量到達かつ idle の
退避候補が無い場合は、退避もフォールバックも拒否もせずそのまま追加のリーダーを
作っていた (ドライバ上限を超過)。上限はハードウェアの事実なので、超過は選択肢に
しない。代わりに、要求が上回っていれば稼働中のリーダーを停止して作り直す。

**帰結として、視聴中のセッションが退避され得る。** 従来この挙動は排他要求だけの
ものだったが、優先度が厳密に上回る要求 (例: 録画 200 が視聴 10 を退避) でも
起きるようになった。退避された側への明示通知は P4 の作業で、それまでは
`reader_alive_check` (2 秒周期) が検知して切断する。

経路ごとの `exclusive` の扱い:

- v2 空間選局 / v1 選局 … クライアントの排他フラグをそのまま渡す。
- 論理チャンネル選局 … BNDP の `SelectLogicalChannel` に排他フラグが無いため
  `false`。優先度で上回れば退避は起きる (`exclusive` はタイの判定のみ)。
- HTTP / Mirakurun … `false`。

`decide_fallback` も優先度を見るようになった。従来は「最初に見つかった idle」を
無条件に退避しており、プライマリドライバなら守られるはずの高優先度 idle が
フォールバック先では落とされていた。

#### 残タスク

- 論理チャンネル選局の候補順序 (DB 優先度順) と `decide()` の再ソート
  (排他数 → 稼働数 → 品質スコア) の統一。現状は前者を保つため候補ループを
  残している。

- `session.rs` から選局系ヘルパ 8 個
  (`try_reuse_existing_set_channel_space_tuner` 等) を削除し、
  `AcquireRequest`/`acquire()` 呼び出しに置き換える。`Session` は要求の
  組み立てと `apply_channel_metadata` 相当の後処理のみ持つ。この際
  `AcquireRequest` に `own_key`/`own_key_will_free_slot`
  (`policy::TuneRequest` 相当) を追加する必要がある。
- **eviction ポリシーの統一** (§2.1-8): 「idle を優先、購読者ありの退避は
  `exclusive` かつ要求優先度が厳密に上回る場合のみ」に一本化し、優先度比較を
  `>` にする。この変更は HTTP 経路にも波及する (`decide()` を共有している
  ため) ―― 上記「要判断・要レビュー」の優先度セマンティクスと合わせて
  再確認すること。
- 検証: `cargo test -p recisdb-proxy` + 実機での連続チャンネル切り替え。

### P3 — 配信経路の高速化 (一部実装)

実装した分:

- **SI 収集をリーダースレッドから退避 (§2.2-10)。** SDT/CDT (ロゴ) と EIT
  (EPG) の収集は、読み取り速度を律速するスレッド上で毎チャンク走っていた。
  クライアントが読むのと同じ broadcast を購読する専用タスク
  (`SharedTuner::spawn_si_collector`) へ移し、読み取り経路を
  「読む・デコードする・配る」だけにした。
  - `subscribe_untracked` で購読する。寄生的な消費者なので、チューナーを
    生かし続けたり keep-alive の勘定を狂わせたりしてはならない。`Weak` を
    持つのでチューナーが落ちれば task も終わり、リーダーが `Running` を
    外れた時点でも止まる。
  - 見えるバイト列が raw からデコード後に変わるが、SI テーブルは
    スクランブルされず、B25 の `strip` が落とすのは null パケットだけなので
    収集結果は変わらない。B25 が使えない経路では元々 raw が流れる。
- **整列済みチャンクの素通し (§2.2-9)。** 前チャンクの残りが無く、188 の倍数
  長で先頭が同期バイトのチャンク (健全なストリームの定常状態) は、再整列用
  carry バッファを経由せずそのまま送る。従来はセッション毎に「carry へコピー →
  carry から取り出しコピー」の 2 回が必ず走っていた。不規則な入力は従来どおり
  carry 経路を通り、パケット境界の正しさは変わらない。
- **エンコーダ使用中は raw 購読を polling しない (§2.2-12)。** 出力は
  エンコーダ側の分岐から出るので、チャンク毎に起こされて捨てるのをやめた。
  購読ハンドル自体は保持する — チューナーの keep-alive / idle-close を
  セッション driven に保つのがその役目のため。broadcast のリングはチャンクを
  1 部だけ保持するので、polling しない受信者がいてもセッション毎に何かが
  積み上がることはない。

#### 残タスク

- **TS 品質解析のチューナー単位集約 (§2.2-11)。** 同一チューナーの N 購読者が
  同じバイト列を N 回解析している。セッション毎の統計 (`packets_dropped` /
  `top_loss_pids` / ビットレート) がセッション固有の解析器に紐付いており、
  共有すると「このセッションが取りこぼした分」と「チューナー全体で落ちた分」の
  区別が失われる。分離設計が要るため見送った。
- **空読み時のスリープ・カウンタ整理 (§2.2-13)。** `wait_ts_stream` の
  戻り値・`n == 0`・`WouldBlock` の 3 状態が同じカウンタに混在している。
  挙動を変えずに整理するには実機での確認が要る。

### P4 — 退避通知と可観測性 (実装済み)

P2b-3 で「優先度が上回れば視聴中のリーダーも停止する」ようになったため、
退避された側が何も知らされないままなのは許容できなくなった。

- **`ReaderState` の変化を `tokio::sync::watch` で配信する。** セッションは
  自分のチューナーの状態を購読し、退避・ドライバ障害を**その瞬間に**知る。
  従来は 2 秒周期の `is_running()` ポーリング (`reader_alive_check`) で、
  最大 2 秒間死んだストリームを掴んだまま、理由不明で切断されていた。
- **`StopReason`** (`Evicted` / `ReaderFailed` / `Released` / `Unspecified`)
  を停止の起点で記録する。退避側 (`acquire::evict_tuner`)、リーダー自身の
  失敗経路、idle-close がそれぞれ設定し、状態が `Stopped` に落ちる**前**に
  書くので、遷移で起こされた購読者は既に理由を読める。セッションはこれを
  ログとセッション履歴の `disconnect_reason` に反映する。
- **決定トレース**: `acquire` が判断 1 件につき 1 行、
  `decision=reuse|create|reject` と key / evict / attempt / priority /
  exclusive / candidates を出力する。従来は 8 個のヘルパが断片的にログを
  出しており、全体像を再構成できなかった。

実装上の注意として、`set_state` は `watch::Sender::send` ではなく
**`send_replace`** を使う。`send` は購読者が 0 のとき**値を更新せずに**エラーを
返すため、リーダーが誰にも見られていない間の遷移が失われ、後から購読した側が
古い状態を見てしまう。リーダーは大半の時間を無購読で過ごす (セッションは
選局を終えて初めて購読する) ので、これは例外ではなく通常のケース。

#### 残タスク

- **プロトコルでの明示通知**。現状クライアント DLL は `ServerMessage::Error`
  を一切処理しておらず、ストリーム中に送っても RPC 応答待ちと誤って突合される
  リスクがあるだけで利点がない。退避理由をクライアントへ届けるには BNDP への
  メッセージ追加と DLL 側の対応が要るため、別作業とする。それまでクライアント
  から見えるのは従来どおり接続断で、変わったのは「即座に切れる」ことと
  「サーバ側に理由が残る」こと。
- 直近の decision trace を `/api` に露出する (ログには出ている)。

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

## 4.5 実機検証 (2026-08-08, PX-MLT5PE / macOS)

macOS 用バックエンド (`bondriver/px4_daemon.rs`) を追加して、実機で確認した結果。
環境: PX-MLT5PE 5 系統、地上波 8 波 + BS 9 波 + CS 12 波受信可、仙台。
`max_instances = 1` × 5 ドライバとして登録 (1 系統 = 1 チャンネルという
ハードウェアの実態と一致)。

| 検証項目 | 結果 | 根拠 |
|---|---|---|
| チャンネルスキャン完走 | OK | 29 物理チャンネルから 106 サービス取得 |
| 単一スロットでの合流 (P1b §6) | OK | 同一サービスへの 2 本目が `decision=reuse`、両方 200 で受信 |
| 同値優先度では奪わない (P2b-3) | OK | 別チャンネル要求が `reject reason=AtCapacity{lowest_idle_priority: None}` → 503 |
| 上限超過で作らない (P2b-3) | OK | 同上。超過生成は発生しない |
| idle 退避 → 別チャンネル可 | OK | 1 本目終了後、`evict_idle_on_path` が idle リーダーを退避し `decision=create` |
| permit 移譲 (P1b §4) | OK | セッションが `max_instances=1` 上でチャンネル切替、`using the caller's own slot permit (same-DLL handoff)` を確認、TS 継続 |
| 決定トレース (P4) | OK | `decision=reuse/create/reject` が 1 判断 1 行で追える |

**未検証**: prewarm タイムアウト後の cold フォールバック (P2a)。再現には
`prewarm_timeout_secs` 経過後の選局という時間依存の条件が要る。

実機テストは常設した:
- `cargo test -p recisdb-proxy --lib px4_daemon -- --ignored` (バックエンド単体)
- `cargo test -p recisdb-proxy --test bndp_hardware -- --ignored` (セッション経路)

---

## 4.6 複数チューナー同時利用のマトリックス試験 (2026-08-08)

5 系統を `max_instances = 1` × 5 ドライバとして登録し、**同時セッション**で
グループ選局を走らせた。単発の試験では出なかった不具合が 2 件出た。

### 見つかった不具合 (修正済み)

1. **`policy.rs` の容量計算が `is_running()` を見ていた。**
   P1a で `count_running_instances_on_driver` 等は `occupies_slot()` に直したが、
   純粋層だけ取り残されていた。`Starting`(BonDriver オープン中、数秒)が
   「空き」と数えられるため、**同時要求が全員同じドライバを選ぶ**。
   5 セッション中 1 本しか通らなかった。`EntryState::occupies_slot()` を追加し、
   容量計数と再利用判定をそちらに寄せた。

2. **リトライが毎回同じ結論に収束していた。**
   同時要求は同じ瞬間にスナップショットを取り、同じ順位付けをするので、
   permit を取れなかった側が再試行しても**また同じドライバ**を選ぶ。
   1 ラウンドにつき 1 本しか進まないのに上限が 3 回固定で、
   5 系統あっても 3 本までしか通らなかった。
   - permit 取得に失敗したパスをその呼び出しの間だけ候補から外す
   - 上限を候補数から算出する (`max_attempts(n) = n + 2`)

   修正後、リトライが 5 ドライバを順に辿ることをログで確認した。

3. **keep-alive の残骸が新規視聴者を弾いていた** (規則の穴)。
   `may_evict` が同値優先度で退避しないため、誰も見ていない keep-alive
   リーダーが満席のグループで新規要求を拒否させていた。keep-alive は
   最適化であって要求ではないので、**idle-close 予約済みの entry は
   優先度に関わらず譲る**ようにした。
   ただし「購読者ゼロ」だけでは足りない ―― `SetChannelSpace` と
   `StartStream` の間の entry も購読者ゼロで、これを奪うと要求元が壊れる。
   `TunerPool::keys_pending_idle_close()` を追加し、
   `EntryState::idle_close_pending` で両者を区別する。

### この環境の同時受信上限

`px4rec` 単体 (プロキシ非経由) で測定:

| 同時本数 | 成功 |
|---|---|
| 2 | 2 |
| 3 | 3 |
| 4 | 2 |
| 5 | 0 |

**3 本が上限**。MacBook Air の USB 帯域か daemon 側の制約と思われる。
5 系統あっても 5 本同時視聴はこの環境では不可能なので、マトリックスの
期待値はこの上限を踏まえて立てる必要がある。

### マトリックス結果 (ハード復旧後)

PX-MLT5PE 5 系統 (`max_instances = 1` × 5 ドライバ、同一グループ)。
各ケースの前にデーモン・プロキシを入れ直し、keep-alive の持ち越しを断つ。

| ケース | 結果 | tuner |
|---|---|---|
| 5 セッション / 5 チャンネル (同時) | 5/5 | 5 |
| 6 セッション / 6 チャンネル (同時) | **5/6** (6 本目を拒否) | 5 |
| 5 セッション / 同一チャンネル (同時) | 5/5 | 5 (下記) |
| 5 セッション / 同一チャンネル (3 秒間隔) | 5/5 | **1** (create=1, reuse=4) |
| 3 チャンネル × 2 視聴 (2 秒間隔) | 6/6 | **3** (create=3, reuse=3) |
| 全系統を優先度 0 で占有 → 優先度 200 で要求 | admit + 視聴者 1 人を切断 | 5 |
| 同一 DLL 上のチャンネル切り替え | TS 継続 | 1 |

- **上限超過は起きない。** 6 本目は `max_instances` を超えて開かず拒否される。
- **合流は効く。** 逐次到着なら 1 チャンネル = 1 チューナーで、残りの系統は空く。
- **優先度退避も効く。** 満席で優先度 200 の要求が通り、退避された視聴者は
  P4 の状態 watch で即座に検知され切断される。

### 同時到着した同一チャンネル要求の合流 (修正済み)

当初、同じチャンネルへの要求が**同時に**到着すると、まだ誰も開いていないため
それぞれが別のドライバに 1 本ずつ開いていた (5 セッションで 5 系統を消費)。
チューナーが潤沢とは限らないので、これは実害のある無駄遣いになる。

`TunerPool::acquire_channel_lock` を追加し、**同一論理チャンネルへの要求を
`acquire` で直列化**した。2 番目以降は 1 番目がエントリを作るまで待ってから
再スナップショットし、合流する。待ちは無駄ではない ―― どのみちその
チューナーが必要になる。

- ロックのキーは要求の候補キー集合。候補の順序が違っても同じキーになる。
  別チャンネル同士は競合しない。
- 取得順序は「チャンネルロック → DLL 初期化ロック」に固定。逆順で取る経路が
  無いのでデッドロックしない。

修正後、同時 5 セッション / 同一チャンネルで `create=1, reuse=4` になった
(修正前は 5 本消費)。

### 検証の過程で見つけたテスト側の誤り (製品側ではない)

一時「同時セッションで TS が届かない」と記録したが、**テストハーネスの
計測窓が短すぎただけ**だった。`StartStreamAck` はリーダーが ready になった
時点で返るが、最初のバイトは BonDriver オープンの落ち着きとセッションの
prefill (既定 1 秒) の後に出る。固定 4 秒のドレインでは、サーバーが現に
送出している最中に 0 バイトと測っていた。バイト数に達するまで待つ形に直した。

同様に「優先度退避で誰も切断されていない」も誤りで、退避された側の
ソケットにバッファ済みフレームが残っていたため「まだ流れている」と
見えていた。EOF で判定するよう直した。**どちらも製品の不具合ではない。**

### 経路別の実機確認 (追加分)

マトリックスに加えて、それまで実機で通していなかった経路を個別に確認した。

| 経路 / パターン | 結果 |
|---|---|
| 排他選局 (`exclusive=true`、同値優先度) | 通過し、視聴者 1 人を退避 |
| `SelectLogicalChannel` (NID/TSID 指定) | 選局・受信とも成立 |
| 切断 → 再接続 (keep-alive 合流) | `create=1, reuse=1` |
| HTTP と BNDP が同一物理チャンネル | `create=1, reuse=1` (1 チューナーを共有) |
| prewarm 失効 (35 秒待機) 後の選局 | cold フォールバックで成立 |

最後の 1 件は P2a のレビューで一度削られかけた分岐で、これでようやく実機の
裏付けが付いた。

| T↔S をまたぐセッション選局 | T→BS→T→CS の 4 段すべてで受信継続 |
| スキャン実行中の選局 | 既存視聴者は受信継続、走査中に 44 回の新規参加が成立 |

T↔S は、レシーバが一度に 1 systemしか開けないためチューナーを閉じて開き直す
唯一の経路 (データソケットも張り直す)。バックエンド単体では確認済みだったが、
セッション経由でも通ることをこれで確認した。

スキャンの試験は判定の作り込みに 3 回失敗している。8 秒待って「スキャン中」と
みなす → 実際には未開始で空振り。「プールのチューナー数が増える」で検知 →
**スキャンは `TunerPool` を経由せず直接 `BonDriverTuner` を開く**ので増えない。
`last_scan` の変化で検知 → これは*完了時*にしか書かれないので窓を 5 分に広げて
ようやく成立した。

### 付随して分かったこと: スキャンはプールの外にいる

`scan_space_blocking` は `BonDriverTuner::new` を直接呼ぶ。つまりスキャンは
スロット permit を取らず、`max_instances` の会計にも `active_tuners` にも
現れない。別ドライバを走査する限り問題ないが、**単一 open のデバイスで、
視聴者が使っているドライバを走査しようとすると衝突する** (EALREADY)。
今回のリファクタで持ち込んだものではなく元からの構造だが、選局側を permit 制に
した今となっては、スキャンだけが例外として残っている。

### まだ実機で試していないもの

- `max_instances > 1` のドライバ (今回の 5 系統はすべて 1)。px4daemon は
  1 系統 = 1 チャンネルなので、意味のある検証には別のドライバ構成が要る
- BNDP v1 の `SetChannel` 経路 (v2 空間選局と論理選局のみ確認)
- 視聴中のドライバを対象にしたスキャン (上記の衝突が実際に起きるかの確認)

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

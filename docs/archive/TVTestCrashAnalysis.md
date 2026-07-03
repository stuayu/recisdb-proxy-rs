# TVTest クラッシュ分析レポート

## 概要

recisdb-proxyサーバーを起動し、bondriver-proxy-clientのDLLをTVTestから実行した際にTVTestがクラッシュする問題の調査結果と修正方針をまとめる。

## 実装状況

| 修正 | 状態 | 説明 |
|------|------|------|
| 修正0: ScanScheduler起動 | **実装済** | main.rsでScanSchedulerを起動するように修正 |
| 修正1: 接続同期 | **実装済** | 固定100ms待機をポーリング+タイムアウトに変更 |
| 修正2: タイムアウト追加 | **実装済** | send_request_with_timeout()を追加 |
| 修正3: GetTsStream検証 | **実装済** | サイズ0チェック、上限制限追加 |
| 修正4: キャッシュ制限 | **実装済** | MAX_SPACES=256, MAX_CHANNELS_PER_SPACE=1024 |

## 既知の制限事項

### チャンネルスキャン機能

サーバー側でのBonDriver直接ロードによるチャンネルスキャンは**未実装**です。

**理由**: recisdbのtunerモジュールはC++ FFIラッパーを使用しており、これをrecisdb-proxyに統合するにはbuild.rsの大幅な変更が必要です。

**回避策**:
1. TVTestなど他のツールでチャンネルスキャンを実行し、結果をDBにインポート
2. クライアント接続時のパッシブスキャンを利用（実装予定）
3. 手動でチャンネル情報をデータベースに登録

## 調査対象ファイル

- `bondriver-proxy-client/src/lib.rs` - DLLエントリーポイント
- `bondriver-proxy-client/src/bondriver/exports.rs` - BonDriverエクスポート関数
- `bondriver-proxy-client/src/bondriver/interface.rs` - vtable定義
- `bondriver-proxy-client/src/client/connection.rs` - TCP接続管理
- `bondriver-proxy-client/src/client/buffer.rs` - リングバッファ
- `recisdb-proxy/src/server/session.rs` - サーバーセッション処理

## 特定されたクラッシュ原因候補

### 0. ScanSchedulerが起動していない (重大度: 高/機能不全)

**場所**: `recisdb-proxy/src/main.rs`

**問題点**:
- `scheduler`モジュールはimportされているが、`ScanScheduler`は実際にインスタンス化・起動されていない
- サーバー起動時にチャンネルスキャンが実行されないため、データベースが空のまま
- クライアントから`EnumTuningSpace`や`EnumChannelName`が呼ばれても`None`を返す

**影響**:
- TVTestがチャンネルリストを取得できない
- ユーザーがチャンネルを選択できない
- 直接的なクラッシュ原因ではないが、機能が使用不能

**サーバーの現状**:
```
EnumTuningSpace → None (空のDBからは何も返せない)
EnumChannelName → None (空のDBからは何も返せない)
GetChannelList → [] (空リスト)
SelectLogicalChannel → Error (チャンネルが見つからない)
SetChannel (space/ch直指定) → Success (チューナーが開ければ動作)
```

### 1. 接続タスク起動タイミングの問題 (重大度: 高)

**場所**: `connection.rs:168-169`

```rust
// Wait for connection to be established
std::thread::sleep(Duration::from_millis(100));
```

**問題点**:
- 固定100ms待機で接続タスクの完了を待っているが、ネットワーク遅延やサーバー応答が遅い場合、接続が確立される前に`send_hello()`が呼ばれる
- 接続未確立状態で`send_request()`を呼ぶと、チャンネルが未初期化でパニックが発生する可能性

**影響**: サーバー接続時にTVTestがクラッシュ

### 2. blocking_recv()のタイムアウト欠如 (重大度: 高)

**場所**: `connection.rs:211`

```rust
match rx.blocking_recv() {
    Some(resp) => Some(resp),
    None => {
        error!("Response channel closed");
        None
    }
}
```

**問題点**:
- `blocking_recv()`にタイムアウトが設定されていない
- サーバーが応答しない場合、TVTestのUIスレッドが無限にブロック
- 結果としてTVTestがフリーズまたは応答なしとなり、ユーザーが強制終了するとクラッシュとして報告される

**影響**: TVTestのUI完全フリーズ

### 3. GetTsStreamでのunsafeスライス作成 (重大度: 中)

**場所**: `exports.rs:177-178`

```rust
let max_size = *size as usize;
let dest = std::slice::from_raw_parts_mut(dst, max_size);
```

**問題点**:
- `max_size`が0の場合の明示的な処理がない
- `dst`ポインタがアライメント不正や無効なメモリを指している場合、未定義動作
- TVTestが不正なパラメータを渡した場合にクラッシュ

**影響**: ストリーム受信時にクラッシュ

### 4. 状態競合条件 (重大度: 中)

**場所**: `connection.rs:128-134`

```rust
pub fn connect(self: &Arc<Self>) -> bool {
    let mut state = self.state.lock();
    if *state != ConnectionState::Disconnected {
        return false;
    }
    *state = ConnectionState::Connecting;
    drop(state);  // ここでロック解放
    // ... 長い処理 ...
```

**問題点**:
- `state`のロックを解放後、Tokioランタイム作成までの間に別スレッドからアクセスされる可能性
- TVTestがマルチスレッドで呼び出す場合、重複接続やリソースリークが発生

**影響**: リソースリーク、予期しない動作

### 5. キャッシュの無制限成長 (重大度: 中)

**場所**: `exports.rs:251-254`, `exports.rs:284-288`

```rust
while state.space_names.len() <= space as usize {
    state.space_names.push(None);
}
```

**問題点**:
- `space`や`channel`が非常に大きな値(例: 0xFFFFFFFF)の場合、大量のメモリを確保しようとしてOOM
- TVTestが順次列挙する場合は問題ないが、不正な値が渡された場合にクラッシュ

**影響**: メモリ枯渇によるクラッシュ

### 6. メモリリーク (重大度: 低)

**場所**: `interface.rs:95`

```rust
std::mem::forget(boxed); // Leak the memory
```

**問題点**:
- `to_static_wide_string()`が意図的にメモリをリーク
- 長時間運用でメモリ使用量が増加
- 現在の実装では`enum_tuning_space()`と`enum_channel_name()`がキャッシュを使用しているため、この関数は使用されていない

**影響**: 長時間運用時のメモリ増加

## 推定されるクラッシュシナリオ

### シナリオA: 接続失敗時のクラッシュ

1. TVTestがDLLをロード
2. `CreateBonDriver()`が呼ばれ、グローバルインスタンス初期化
3. `OpenTuner()`が呼ばれる
4. `connect()`内で100ms待機後、`send_hello()`を呼ぶ
5. 接続タスクがまだサーバーに接続していない状態で、リクエスト送信を試みる
6. チャンネルの状態不整合でパニック発生
7. TVTestクラッシュ

### シナリオB: サーバー無応答時のフリーズ

1. サーバーに接続成功
2. `SetChannel2()`が呼ばれる
3. `send_request()`でサーバーにリクエスト送信
4. サーバーが何らかの理由で応答を返さない
5. `blocking_recv()`が無限待機
6. TVTestのUIスレッドがブロック
7. ユーザーが強制終了

## 修正方針

### 修正0: ScanSchedulerの起動 (優先度: 高)

**変更箇所**: `recisdb-proxy/src/main.rs`

```rust
// 現在: schedulerモジュールはimportのみ
mod scheduler;

// 修正案: ScanSchedulerを起動
use scheduler::ScanScheduler;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ... 既存の初期化処理 ...

    // ScanSchedulerを作成・起動
    let scan_scheduler = ScanScheduler::new(
        db.clone(),
        tuner_pool.clone(),  // TunerPoolへの参照が必要
    );

    // バックグラウンドタスクとして起動
    let scheduler_handle = tokio::spawn(async move {
        scan_scheduler.run().await;
    });

    // サーバー起動
    let server = Server::new(config);
    server.run().await?;

    Ok(())
}
```

**追加検討事項**:
- サーバー起動時に初回スキャンを強制実行するオプション (`--scan-on-start`)
- コマンドラインからスキャンを手動トリガーするサブコマンド (`recisdb-proxy scan`)
- クライアントプロトコルにスキャンリクエストを追加

### 修正1: 接続確立の適切な同期 (優先度: 高)

**変更箇所**: `connection.rs`

```rust
// 現在
std::thread::sleep(Duration::from_millis(100));

// 修正案: チャンネルを使った同期
// 接続タスクからの準備完了通知を待つ
let (ready_tx, ready_rx) = std::sync::mpsc::channel();
runtime.spawn(async move {
    // 接続処理
    let _ = ready_tx.send(result);
});

match ready_rx.recv_timeout(config.connect_timeout) {
    Ok(true) => { /* 接続成功 */ }
    Ok(false) | Err(_) => { /* 接続失敗またはタイムアウト */ }
}
```

### 修正2: blocking_recv()にタイムアウト追加 (優先度: 高)

**変更箇所**: `connection.rs:211`

```rust
// 現在
match rx.blocking_recv() {

// 修正案: recv_timeout使用
// ただしmpsc::Receiverにはblocking_recv_timeoutがないため、
// 別の同期プリミティブを使用するか、async内でtokio::time::timeoutを使用

// 方法1: tokio::sync::oneshot + timeout
let (tx, rx) = tokio::sync::oneshot::channel();
let result = tokio::time::timeout(
    self.config.read_timeout,
    rx
).await;

// 方法2: crossbeamのchannelを使用(recv_timeout対応)
```

### 修正3: GetTsStreamの入力検証強化 (優先度: 中)

**変更箇所**: `exports.rs:161-193`

```rust
pub unsafe extern "system" fn get_ts_stream(
    _this: *mut c_void,
    dst: *mut BYTE,
    size: *mut DWORD,
    remain: *mut DWORD,
) -> BOOL {
    // 追加: サイズの検証
    if dst.is_null() || size.is_null() || remain.is_null() {
        return 0;
    }

    let max_size = *size as usize;
    if max_size == 0 {
        *remain = 0;
        return 0;
    }

    // サイズ上限の設定（バッファオーバーフロー防止）
    let max_size = max_size.min(RING_BUFFER_SIZE);

    // ... 以下既存処理
}
```

### 修正4: キャッシュサイズ制限 (優先度: 中)

**変更箇所**: `exports.rs:251`, `exports.rs:284`

```rust
// 追加: 上限チェック
const MAX_SPACES: usize = 256;
const MAX_CHANNELS: usize = 1024;

if space as usize >= MAX_SPACES {
    return std::ptr::null();
}

if channel as usize >= MAX_CHANNELS {
    return std::ptr::null();
}
```

### 修正5: 接続状態の適切なロック保持 (優先度: 低)

**変更箇所**: `connection.rs:128-179`

接続処理全体でロックを保持するか、より細かい状態管理を実装する。ただし、パフォーマンスへの影響を考慮する必要がある。

## 推奨実装順序

1. **修正0**: ScanScheduler起動 - チャンネルリスト取得に必須
2. **修正1**: 接続確立の同期 - 最も可能性が高いクラッシュ原因
3. **修正2**: タイムアウト追加 - フリーズ防止
4. **修正3**: 入力検証 - 防御的プログラミング
5. **修正4**: キャッシュ制限 - OOM防止
6. **修正5**: 状態管理改善 - 安定性向上

## デバッグ方法

### ログ出力の有効化

環境変数を設定してログを有効化:

```cmd
set RUST_LOG=debug
```

または、INIファイルと同じディレクトリに`env_logger`が参照する設定を配置。

### TVTest側の確認

1. TVTestの「設定」→「一般」→「BonDriver」でDLLの読み込み状況を確認
2. TVTestのログファイルを確認（存在する場合）
3. Windowsイベントビューアでアプリケーションエラーを確認

### サーバー側の確認

```cmd
set RUST_LOG=debug
recisdb-proxy --listen 0.0.0.0:12345
```

サーバーログで接続試行やプロトコルエラーを確認。

## 参考情報

### BonDriverインターフェース

- IBonDriver: 基本インターフェース
- IBonDriver2: チャンネル列挙機能追加
- IBonDriver3: LNB制御機能追加

### vtableレイアウト

```
IBonDriver3Vtbl:
  base (IBonDriver2Vtbl):
    base (IBonDriverVtbl):
      query_interface, add_ref, release,
      open_tuner, close_tuner, set_channel,
      get_signal_level, wait_ts_stream, get_ready_count,
      get_ts_stream, purge_ts_stream
    get_tuner_name, is_tuner_opening, enum_tuning_space,
    enum_channel_name, set_channel2, get_cur_space, get_cur_channel
  set_lnb_power
```

### プロトコル概要

- マジックバイト: "BNDP" (4バイト)
- フレーム: [Magic(4)][Length(4)][Type(2)][Payload(可変)]
- ヘッダーサイズ: 10バイト
- 最大フレームサイズ: 16MB

# クライアントDLL (bondriver-proxy-client) レビュー台帳 — 2026-08

`BonDriver_NetworkProxy.dll` の実装を通しで精査した結果の指摘と対応状況。

対象は多段構成 (上流 recisdb-proxy が本DLLを BonDriver として開く) と、
拠点間WAN中継 (宮城・福島 ↔ 東京) の運用を前提とした評価。
ローカルLANの単段構成では顕在化しない指摘が多く含まれる。

状態の凡例: **未対応** / **対応済み** / **見送り**(理由を併記)

---

## 重大

「一度この状態になるとプロセスを再起動しないと復旧しない」類。

### C-1. 初回接続に失敗したインスタンスは永久に死ぬ — 未対応

`Connection::connect()` は `state != Disconnected` で即 false を返す。
接続失敗時の state は `Error` になり、`Error` → `Disconnected` に戻すのは
`disconnect()` だけで、それを呼ぶのは `Release` / `Drop` のみ。

`open_tuner` は state が `Disconnected` のときしか `connect()` を呼ばないため、
**2回目以降の `OpenTuner` は永久に 0 を返す**。TVTest の再試行は無意味で、
プロセス再起動が必要になる。

拠点間運用では「起動時にたまたま対向が落ちていた」だけでこの状態に入る。

### C-2. RPC応答に対応IDがなく、1回のタイムアウトで以後ずっと1つズレる — 未対応

`send_request_with_timeout` は応答チャネルの先頭を1つ取り出すだけで、
どのリクエストに対する応答かを検証しない。タイムアウトで抜けた後に遅れて
届いた応答はチャネルに残り、**次のリクエストがそれを受け取る**。以後ずっと
1つズレたままになり、`SetChannel` などが全て失敗するようになる。

再接続時に排出しているのは `req_rx` (送信待ちリクエスト) だけで、応答側の
滞留は掃除していない。

### C-3. TCP keepalive がなく、WANブラックホールで無限ハングする — 未対応

ソケットには `set_nodelay` しか設定していない。NAT のセッションテーブルが
黙って落ちる、経路が消えるといったケースでは FIN も RST も来ないため、
受信ループは永久に待ち続ける。**EOF もエラーも発生しないので再接続
スーパーバイザが起動しない**。アプリケーション層のハートビートもない。

結果として「TS が止まったまま、接続は生きているように見え、復旧しない」。
拠点間中継で最も踏みやすい。

### C-4. 再接続後にリングバッファを捨てていない — 未対応

`restore_session` は Hello → OpenTuner → SetChannel → StartStream を張り直す
が、リングバッファを purge しない。断線前の古い TS が先頭に残り、その後ろに
復帰後の TS が連結されるため、**復帰のたびに不連続が発生**して Drop / Error
として観測される。WAN でリンクが上下するたびに起きる。

---

## 中

### C-5. `GetTsStream`(コピー版)が `*size` を入力容量として信用する — 未対応

BonDriver 仕様上 `pdwSize` は OUT 専用で、呼び出し側バッファの容量を知る
手段はない。実装はこれを入力容量として扱っており、コード中のコメント自身が
「TVTest は 0 かゴミを渡す」と認めている。ゴミが大きい値だと最大 64KB を
書き込むため、**ホスト側のバッファがそれより小さければヒープを壊す**。

ptr 版 (`C_GetTsStream2`) が安全な経路であり、recisdb-proxy 側は
CLAUDE.md の不変条件でそちらを必須にしているが、vtable にコピー版を出して
いる以上ホストの実装次第で踏まれる。

### C-6. `GetModuleHandleExW` が参照カウントを増やしたままになる — 未対応

`config.rs` の呼び出しに `GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT` が
付いていない。**DLL がアンロードできなくなる**。しかもインスタンスごとに
`load_config()` が走るようになったため、回数分だけ加算される。
`logging.rs` の同等の呼び出しには正しくフラグが付いており、config.rs 側の
抜けだった。

### C-7. 選局が最大3RPC直列で、その間インスタンスロックを保持する — 未対応

`SetChannel2` は `SetChannelSpace` + `PurgeStream` + `StartStream` の3RPCを
直列に投げる。各々のタイムアウトは `ReadTimeout` (ini 既定 30 秒)。
さらにその間ずっとインスタンスの Mutex を保持するため、`GetTsStream` も
同じロックで待たされる。最悪 90 秒、TVTest の UI とストリームスレッドが
両方固まる。

### C-8. ログが無制限に増える — 未対応

追記モードのみで、ローテーションもサイズ上限も保持日数もない。
`LogLevel=debug` では `WaitTsStream` が呼び出しごとに1行吐く。
サーバ側には `retention_days` があるのにクライアント側には何もない。

### C-9. ini フォールバックのファイル名が固定 — 未対応

モジュールパスの取得に失敗したときのフォールバックが
`"BonDriver_NetworkProxy.ini"` 決め打ちのため、DLL をコピーして使う運用
(`BonDriver_NetworkProxy_T0.dll` など) では**別チューナーの ini を読む**。
EDCB の多チューナー構成で誤った接続先に繋がる。

### C-10. `GetModuleFileNameW` のバッファが 260 — 未対応

長いパスでは切り詰められ、`ERROR_INSUFFICIENT_BUFFER` も見ていないため、
ini を見失って既定値 (127.0.0.1) で動いてしまう。`logging.rs` 側は 32768 を
確保しており、ここも不一致だった。

### C-11. リングバッファのサイズがコメントと5倍違う — 未対応

`RING_BUFFER_SIZE` のコメントは「100 MB」だが実際は `188*1024*100` =
**18.4 MiB**。16 Mbps で約 9 秒ぶん。拠点間のバッファ設計を見積もる際の
基準値が5倍ずれていた。

---

## 低

| # | 指摘 | 状態 |
|---|---|---|
| C-12 | `remain` をバイト数で返す。BonDriver の慣例は残りパケット数。TVTest は非0判定にしか使わないので実害はないが値としては誤り | 未対応 |
| C-13 | `ConnectTimeout` の既定値が ini 経路 5 秒 / `ConnectionConfig::default()` 10 秒で不一致 | 未対応 |
| C-14 | `TsRingBuffer::read()` はラップ時に末尾までしか返さず `read_into` と挙動が違う。現在未使用だが誤用しやすい | 未対応 |
| C-15 | `BonDriverState::tuner_name` は死にフィールド (`GetTunerName` は静的を返す) | 未対応 |
| C-16 | インスタンスごとに tokio runtime (worker 2)。EDCB 8 チューナーで 16 スレッド + 8×18.4MiB のリングバッファ | 未対応 |
| C-17 | `instance_of` が毎 FFI コールで Mutex + HashSet を引く。`GetTsStream` もホットパス | 未対応 |
| C-18 | `load_from_ini` は `[Server]` セクションがないと黙って環境変数/既定値に落ちる。設定ミスに気づけない | 未対応 |

---

## 運用上の注意 (コードの不具合ではないもの)

- **中継段・録画は `StreamClass = record` を明示する。** ini の既定は `view` で、
  VIEW は輻輳時にフレームを黙って破棄する。破棄されたデータは回復しても
  戻らないため、録画ファイルに穴が空く。RECORD は「穴を空けるくらいなら
  切断する」ので、ファイルは途中で終わることはあっても連続性は保たれる。
- **バッファはジッタしか吸収しない。** 実効帯域がストリームのビットレートを
  下回る拠点では、秒数を伸ばしても遅延が単調増加して最後に切れるだけ。
  `ServiceFilter = single` かエンコード配信でレート自体を下げること
  (`docs/STREAMING_DESIGN.md` §3.2 参照)。

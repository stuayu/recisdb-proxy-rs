# クライアントDLL (bondriver-proxy-client) レビュー台帳 — 2026-08

`BonDriver_NetworkProxy.dll` の実装を通しで精査した結果の指摘と対応状況。

対象は多段構成 (上流 recisdb-proxy が本DLLを BonDriver として開く) と、
拠点間WAN中継を前提とした評価。
ローカルLANの単段構成では顕在化しない指摘が多く含まれる。

状態の凡例: **未対応** / **対応済み** / **見送り**(理由を併記)

---

## 重大

「一度この状態になるとプロセスを再起動しないと復旧しない」類。

### C-1. 初回接続に失敗したインスタンスは永久に死ぬ — 対応済み

`Connection::connect()` は `state != Disconnected` で即 false を返す。
接続失敗時の state は `Error` になり、`Error` → `Disconnected` に戻すのは
`disconnect()` だけで、それを呼ぶのは `Release` / `Drop` のみ。

`open_tuner` は state が `Disconnected` のときしか `connect()` を呼ばないため、
**2回目以降の `OpenTuner` は永久に 0 を返す**。TVTest の再試行は無意味で、
プロセス再起動が必要になる。

拠点間運用では「起動時にたまたま対向が落ちていた」だけでこの状態に入る。

**対応**: 失敗した接続は `Error` に留めず、後始末して `Disconnected` に戻す。
`connect()` は `Error` からの再試行も受け付け、`OpenTuner` は
`Disconnected` / `Error` のどちらからも接続を試みる。

### C-2. RPC応答に対応IDがなく、1回のタイムアウトで以後ずっと1つズレる — 対応済み

`send_request_with_timeout` は応答チャネルの先頭を1つ取り出すだけで、
どのリクエストに対する応答かを検証しない。タイムアウトで抜けた後に遅れて
届いた応答はチャネルに残り、**次のリクエストがそれを受け取る**。以後ずっと
1つズレたままになり、`SetChannel` などが全て失敗するようになる。

再接続時に排出しているのは `req_rx` (送信待ちリクエスト) だけで、応答側の
滞留は掃除していない。

**対応**: プロトコルは変えず、2つのガードで自己修復させる。
(1) 送信前に滞留している応答を捨てる (放棄されたリクエストのものしかない)。
(2) 受信した応答が、いま送ったリクエストに対応し得る種別かを検証し、
違えば読み飛ばして待ち続ける。`Error` はどのリクエストにも対する応答として
受け付ける (サーバが拒否を返す唯一の手段のため)。

### C-3. TCP keepalive がなく、WANブラックホールで無限ハングする — 対応済み

ソケットには `set_nodelay` しか設定していない。NAT のセッションテーブルが
黙って落ちる、経路が消えるといったケースでは FIN も RST も来ないため、
受信ループは永久に待ち続ける。**EOF もエラーも発生しないので再接続
スーパーバイザが起動しない**。アプリケーション層のハートビートもない。

結果として「TS が止まったまま、接続は生きているように見え、復旧しない」。
拠点間中継で最も踏みやすい。

**対応**: 2段構えにした。

- TCP keepalive を有効化 (idle 15 秒 / 間隔 5 秒 / 再試行 3 回。Windows は
  再試行回数を他の2値から導出するため設定しない)。
- ストリーミング中に 20 秒データが来なければリンク死とみなして切断し、
  再接続経路に乗せる。keepalive だけでは「経路は生きているがサーバが
  流していない」ケースと、中間の NAT 機器が代理応答するケースを拾えない。
  アイドル中のチューナーは無通信が正常なので、**StartStream 済みのときだけ**
  この監視を有効にする。

**追補 (2026-08-21)**: この 20 秒監視が、起動の遅いチャンネルを殺していた。
BS4K (`stream_format = 'mmttlv'`) はサーバ側で外部 MMT/TLV 変換器
(dantto4k CLI) を起動してから TS になるため、最初の 1 バイトが出るまで
20 秒前後かかる。つまり定常状態のしきい値のまま起動を監視すると、
再生が始まる直前に切断→再接続→また変換器の起動待ち、というループに
なりうる。**最初の TS が届くまでは `FIRST_DATA_GRACE` (60 秒) を使い、
1 チャンクでも届いたら `STREAM_IDLE_TIMEOUT` (20 秒) に戻す**ようにした。
猶予は StartStream 時だけでなく、**選局 (SetChannel / SetChannelSpace /
SelectLogicalChannel) を送った時にも張り直す** — 切り替え先が 4K なら
同じ起動待ちが再び発生するため。猶予期間中に届いた制御フレーム
(各種 Ack、信号レベル) は猶予を延長しない (延長すると、TS を流さない
サーバに対して無限に待つことになる)。

### C-4. 再接続後にリングバッファを捨てていない — 対応済み

`restore_session` は Hello → OpenTuner → SetChannel → StartStream を張り直す
が、リングバッファを purge しない。断線前の古い TS が先頭に残り、その後ろに
復帰後の TS が連結されるため、**復帰のたびに不連続が発生**して Drop / Error
として観測される。WAN でリンクが上下するたびに起きる。

**対応**: `restore_session` の冒頭でリングバッファを捨てる。サーバ側は
どのみちストリームを張り直すので、断線前のデータに保存する価値はない。

---

## 中

### C-5. `GetTsStream`(コピー版)が `*size` を入力容量として信用する — 対応済み

BonDriver 仕様上 `pdwSize` は OUT 専用で、呼び出し側バッファの容量を知る
手段はない。実装はこれを入力容量として扱っており、コード中のコメント自身が
「TVTest は 0 かゴミを渡す」と認めている。ゴミが大きい値だと最大 64KB を
書き込むため、**ホスト側のバッファがそれより小さければヒープを壊す**。

ptr 版 (`C_GetTsStream2`) が安全な経路であり、recisdb-proxy 側は
CLAUDE.md の不変条件でそちらを必須にしているが、vtable にコピー版を出して
いる以上ホストの実装次第で踏まれる。

**対応**: ゴミと本物の容量を区別する手段がないので、**どんな呼び出し側でも
確実に収まる量 = TSパケット1個**を上限にした。TS用のバッファを1パケット未満で
渡すホストは存在しない。`pdwRemain` は残量を返し続けるので、ループする
呼び出し側の総スループットは変わらない (呼び出し回数が増えるだけ)。
スループットが要るホストは ptr 版を使うべきで、その旨を初回に一度ログへ出す。

### C-6. `GetModuleHandleExW` が参照カウントを増やしたままになる — 対応済み

`config.rs` の呼び出しに `GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT` が
付いていない。**DLL がアンロードできなくなる**。しかもインスタンスごとに
`load_config()` が走るようになったため、回数分だけ加算される。
`logging.rs` の同等の呼び出しには正しくフラグが付いており、config.rs 側の
抜けだった。

**対応**: `GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT` を追加。

### C-7. 選局が最大3RPC直列で、その間インスタンスロックを保持する — 対応済み

`SetChannel2` は `SetChannelSpace` + `PurgeStream` + `StartStream` の3RPCを
直列に投げる。各々のタイムアウトは `ReadTimeout` (ini 既定 30 秒)。
さらにその間ずっとインスタンスの Mutex を保持するため、`GetTsStream` も
同じロックで待たされる。最悪 90 秒、TVTest の UI とストリームスレッドが
両方固まる。

**対応**: ネットワーク I/O を行う全エクスポート (OpenTuner / CloseTuner /
SetChannel / SetChannel2 / EnumTuningSpace / EnumChannelName / PurgeTsStream /
SetLnbPower) で、`Connection` を取り出したらロックを解放してから RPC を
投げ、結果の反映時に取り直すようにした。選局中も `GetTsStream` は待たされ
ない。

タイムアウト自体は縮めていない。サーバ側の選局リトライ
(`set_channel_retry_timeout_ms`) は正当に数秒かかることがあり、WAN 越しでは
さらに伸びるため、短縮すると正常な選局を失敗扱いにしてしまう。問題だったのは
待ち時間そのものではなく、待っている間に無関係な呼び出しを巻き込んでいた点。

### C-8. ログが無制限に増える — 対応済み

追記モードのみで、ローテーションもサイズ上限も保持日数もない。
`LogLevel=debug` では `WaitTsStream` が呼び出しごとに1行吐く。
サーバ側には `retention_days` があるのにクライアント側には何もない。

**対応**: サイズ上限でのローテーションを追加 (8MB × 3世代)。
Windows は開いたままのファイルを rename できないため、ローテーション時は
必ず先にハンドルを閉じる。rename に失敗しても必ず開き直すので、ログが黙って
止まることはない。

### C-9. ini フォールバックのファイル名が固定 — 対応済み

モジュールパスの取得に失敗したときのフォールバックが
`"BonDriver_NetworkProxy.ini"` 決め打ちのため、DLL をコピーして使う運用
(`BonDriver_NetworkProxy_T0.dll` など) では**別チューナーの ini を読む**。
EDCB の多チューナー構成で誤った接続先に繋がる。

**対応**: ini 名は常に DLL のファイル名から導出する。カレントディレクトリを
探すフォールバックも同じ名前で探し、モジュールパスが取れたのに ini が
見つからない場合は既定値に落ちる (別 DLL の ini を拾わない)。

### C-10. `GetModuleFileNameW` のバッファが 260 — 対応済み

長いパスでは切り詰められ、`ERROR_INSUFFICIENT_BUFFER` も見ていないため、
ini を見失って既定値 (127.0.0.1) で動いてしまう。`logging.rs` 側は 32768 を
確保しており、ここも不一致だった。

**対応**: バッファを 32768 に拡張し、戻り値がバッファ長に達したら切り詰め
とみなして採用しない。

なお C-6 / C-9 / C-10 はいずれも Windows 専用の分岐で、意味のある純ロジック
が残らないため単体テストは追加していない (`x86_64-pc-windows-gnu` の型検査
のみ)。

### C-11. リングバッファのサイズがコメントと5倍違う — 対応済み

`RING_BUFFER_SIZE` のコメントは「100 MB」だが実際は `188*1024*100` =
**18.4 MiB (19.25 MB)**。16 Mbps で約 9.6 秒ぶん。拠点間のバッファ設計を
見積もる際の基準値が5倍ずれていた
(`docs/STREAMING_DESIGN.md` の「19.2MB」は10進 MB 表記で正しい)。

**対応**: コメントを実値に直し、定数と記載がまた乖離しないようテストで
固定した。

---

## 低

| # | 指摘 | 状態 |
|---|---|---|
| C-12 | `remain` をバイト数で返す。BonDriver の慣例は残りパケット数 | **見送り** (下記) |
| C-13 | `ConnectTimeout` / `ReadTimeout` の既定値が ini 経路と `ConnectionConfig::default()` で不一致 | 対応済み |
| C-14 | `TsRingBuffer::read()` はラップ時に末尾までしか返さず `read_into` と挙動が違う | 対応済み |
| C-15 | `BonDriverState::tuner_name` は死にフィールド (`GetTunerName` は静的を返す) | 対応済み |
| C-16 | インスタンスごとに tokio runtime (worker 2)。EDCB 8 チューナーで 16 スレッド + 8×18.4MiB のリングバッファ | **見送り** (下記) |
| C-17 | `instance_of` が毎 FFI コールで Mutex + HashSet を引く。`GetTsStream` もホットパス | 対応済み |
| C-18 | `load_from_ini` は `[Server]` セクションがないと黙って環境変数/既定値に落ちる | 対応済み |

対応の内訳:

- **C-13**: `ConnectionConfig::default()` をサンプル ini が記載する値
  (接続 5 秒 / 受信 30 秒) に合わせ、ini・環境変数の各ローダーはそこへ
  フォールバックする。既定値の定義箇所を1つにして乖離しないようにした。
- **C-14**: `read()` が返すのは「連続領域」であって「利用可能な全量」では
  ないことをドキュメント化し、変数名も `contiguous` に改めた。挙動は変えて
  いない (現状の呼び出し側はこの契約で正しい)。
- **C-15**: フィールドを削除。
- **C-17**: `RwLock` に変更。全 FFI コールが読み取り、書き込みは
  生成/解放時のみ。
- **C-18**: ini はあるが `[Server]` がない場合を error でログに出す。

見送りの理由:

- **C-12**: 単位をパケット数に変えると、サーバ側 `bondriver/windows.rs` が
  `pending.len()` (バイト) と足している箇所が単位混在になる。値は「まだ残りが
  あるか」の判定にしか使われておらず、実害がない一方で変更は本体側にも波及
  するため、現状の単位を明記するに留める。
- **C-16**: 共有ランタイム化には、インスタンス単位のタスク停止を
  `disconnect()` から安全に行う仕組みが要る (今は runtime ごと落として
  いる)。FFI 解放経路の安全性に直結する割に、得られるのはスレッド数の削減
  だけ。EDCB 8 チューナーでも 16 スレッドで、実害が観測されてから着手する。

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

---

## 2026-09-28 実運用ログ追補: 多段 BonDriverProxyEx 接続

匿名化した実運用ログで同一メッセージを集計し、代表的な時系列を確認した。ログは旧版DLLの期間を含むため、旧版にしかない文言と現ソースを分けて判定する。

### パターン別判定

1. **Streaming中の `SetChannel2` — クライアントDLL不具合、対応済み**

   匿名化ログで `SetChannelSpace` 後も `state=Streaming` なのに `StartStream` を再送し、`StartStream skipped` と対になっていた。旧実装の `bondriver-proxy-client/src/bondriver/exports.rs` の `set_channel2` が常に `start_stream()` を呼び、失敗しても `SetChannel2` を成功返却していた。現在はStreaming中の再送を抑止し、非Streaming時だけ開始する。開始失敗は0を返す (`exports.rs:913-957`, `connection.rs:926-991`)。

2. **`CloseTuner` 後のEOF・再接続 — クライアントDLL不具合、対応済み**

   複数の匿名化ログで `last_request=CloseTuner` の直後にEOFや `RPC CloseTuner interrupted` が発生した。現サーバーは `recisdb-proxy/src/server/session.rs:1552-1559` で cleanup後に `CloseTunerAck` を送るため、EOF自体は旧版/リンク断を含む切断事象であり、閉じたチューナーを復活させるべきではない。旧クライアントは close 中も `closing=false` のため supervisor が再接続した。現在は `CloseTuner` を明示的な接続境界として supervisor を止め、RPC後に transportも破棄する。CloseTuner待ちのリンク断とEOFは通常診断へ降格 (`connection.rs:652-669`, `connection.rs:781-793`, `connection.rs:1780-1783`)。

3. **SetChannelSpace拒否後のrestoreループ — 一時的排他競合を考慮した無期限再試行へ修正**

   `server rejected SetChannelSpace on reconnect` は、再接続はできたが排他選局を再取得できなかった状態。BonDriverProxyEx/TVTest/EDCBは途中で `OpenTuner` を呼び直さないため、サーバー再起動や録画による一時的競合でrestoreを打ち切るとストリームが恒久停止する。restore失敗時はsession contextを保持し、attemptをリセットせず既存の指数backoff（30秒上限）で無期限再試行する。WARNは1回目と連続失敗回数が2の冪（2,4,8...）だけに間引き、成功時は失敗回数をINFOで1行出す (`connection.rs:1084-1086`, `connection.rs:1525-1556`)。

4. **`exclusive=true` の `SetChannelSpace`拒否 — 正常な候補探索/環境・設定起因、ログ対応済み**

   `Priority=10`, `Exclusive=1` のINIで拒否が発生し、同一時刻に複数channelを順に試していた。BonDriverProxyEx が空きチューナーを探す過程の拒否と整合する。サーバーの `handle_set_channel_space` は `recisdb-proxy/src/server/session.rs:1937-2150` で単一の acquire経路へ渡し、候補不足/排他競合をfalse Ackにする契約。選局結果はfalseのまま維持し、クライアントの拒否診断だけ `debug` へ降格した (`connection.rs:858-889`, `exports.rs:963-970`)。

5. **`last_request=none` のEOF・接続拒否 — 主因はWAN/サーバー停止など環境起因、ログ/再接続制御を対応**

   `client_state=Connected` のEOFとWinSock 10061が続き、`last_request=GetSignalLevel` や `none` のEOFも短い間隔で発生した。旧実装にも500ms開始・30秒上限のbackoffはあったが、再接続に成功するたび `attempt=0` へ戻るため、すぐ落ちるフラップでは500ms周期へ戻った。現在は10秒以上安定した接続だけbackoffを初期化し、短時間フラップは指数backoffを継続する。server EOFのファイルログはERRORからWARNへ降格 (`connection.rs:1063-1089`, `connection.rs:1587-1595`, `connection.rs:1780-1783`)。

6. **`WaitTsStream: start_stream failed` — クライアントDLLの再試行/ログ量産、対応済み**

   クライアントPCのログで `RPC Hello timed out`、`initial connection failed` と同じ時間帯に、サーバー未接続/チューナー未準備中にWaitTsStreamがポーリングごとにStartStreamを送っていた。接続失敗自体は環境起因だが、毎回送信・WARN出力はクライアント側。現在はStreaming状態を冪等扱いし、StartStream失敗後1秒間はRPCとWARNを抑止する (`connection.rs:197-205`, `connection.rs:926-991`, `exports.rs:320-331`)。

7. **`GetModuleHandleW ... trying NULL` — 旧版DLLのログレベル/実装問題、現ソース対応済み**

   DLL名検索失敗後にNULLへフォールバックしている旧版ログがあった。NULLはホストEXEのベースを返し、RTTI RVA計算を壊すため安全なフォールバックではない。現ソースはDLL内アドレスから `GetModuleHandleExW` を引き、`UNCHANGED_REFCOUNT` も指定し、失敗時は0で停止する (`bondriver-proxy-client/src/bondriver/exports.rs:1264-1308`)。現ソースに旧文言はなく、ERRORを降格しない。旧版DLLの差し替えが必要。

8. **`Buffer full, dropped` — 実データ欠損は環境/消費側、ログ量産はクライアント対応済み**

   約100ms間隔で大きなTSチャンクを取りこぼしており、WARNの問題だけではなく、受信帯域・TVTest/上流Proxyの読み出し遅延・単一サービス化なし等の実運用条件でリングバッファが満杯になった実データ欠損。`TsRingBuffer::write` は欠損を正しく数えており、設定だけで復元できない。現在はドロップ累計を保持したまま、警告を1秒に1回へ集約する (`buffer.rs:94-113`, `connection.rs:1269-1278`)。

### 変更と検証

- `bondriver-proxy-client/src/client/connection.rs`: CloseTuner後の再接続抑止、restore無期限再試行・指数backoff・冪ログ、安定接続ベースのbackoff、StartStream冪等化/1秒再試行抑止、EOFログ降格。
- `bondriver-proxy-client/src/bondriver/exports.rs`: Streaming中のSetChannel2でStartStreamを再送しない。StartStream失敗を成功返却しない。候補拒否とWaitTsStreamログの量を抑制。
- `bondriver-proxy-client/src/client/buffer.rs`: ドロップ警告のインスタンス単位レート制限。
- 単体テスト: restore失敗ログ間引き、安定接続backoff、Streaming中のStartStream冪等性、失敗後の再試行抑止を追加。
- `cargo test -p bondriver-proxy-client`: 38 passed。

### 未対応・運用作業

- 実機DLL、BonDriverProxyEx、WAN断線を伴うWindows実動作は未検証。Windows向けリリースビルドと実機で `SetChannel2`、CloseTuner、再接続、TS欠損を確認する必要がある。
- 旧DLLの該当ログを消すには、修正版 `BonDriver_NetworkProxy_*.dll` をクライアントへ差し替える。INI変更は不要。`Exclusive=1` は排他要求そのものなので、候補拒否をなくすにはサーバー側の空き容量確保、同時利用削減、または運用要件に応じた `Exclusive=0` の判断が必要。
- 8番の欠損を直すには、実効帯域をTS bitrate以上にし、必要ならINIの `ServiceFilter=single`、中継段の `StreamClass=record`、またはエンコード配信を使う。ログレート制限は欠損自体を隠さない。

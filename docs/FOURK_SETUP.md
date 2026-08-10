# BS4K (高度BSデジタル) 対応

## 方式: dantto4k の BonDriver ラッパーを噛ませる

4K放送は MMT/TLV 多重で、MPEG-2 TS ではない。recisdb-proxy のパイプライン
(B25・TS解析・EPG・チャンネル列挙・録画) はすべて TS 前提なので、**TS に
変換してから proxy に入れる**。

変換は [dantto4k](https://github.com/nekohkr/dantto4k) (Apache-2.0) が
同梱する `BonDriver_dantto4k.dll` で行う。これは別の BonDriver を内側に
ロードし、ACAS 復号 + MMT/TLV→MPEG-2 TS 変換をリアルタイムで行って返す
BonDriver。

```
4Kチューナー → BonDriver_BDA 等 → BonDriver_dantto4k.dll → recisdb-proxy
                                    ↑ここでTSになる
```

recisdb-proxy から見れば**ただの BonDriver** なので、DB に登録するだけでよい。
proxy 側にソースの取り込みや FFI 結合は不要。

補足: dantto4k は**復調しない**。復調はチューナーのハードと内側の BonDriver が
行う。dantto4k がやるのは復号と多重方式の変換。

## セットアップ

1. dantto4k を配置し、`BonDriver_dantto4k.ini` で内側の BonDriver
   (4Kチューナーのもの) を指定する
2. ACAS カードを PC/SC で読める状態にする (または dantto4k の casproxyserver)
3. `BonDriver_dantto4k.dll` のパスを recisdb-proxy の BonDriver として登録
4. スキャンを実行

複数チューナーで使う場合は、DLL とその `.ini` をチューナーごとにコピーして
別名にする (`BonDriver_dantto4k_T0.dll` など)。`max_instances` は 1 のままに
しておく。

## 実装済みの対応

### チャンネル分類 (`classify_nid`)

変換後の TS は**元の network_id 0x000B を保持している**。実機で確認した値:

```
TSID 0xB070 / NID 0x000B (高度BSデジタル放送)
サービス (BS朝日4K): SID 0x97 / service_type 0x01
  映像 PID 0x100 stream_type 0x24 (H.265)
  音声 PID 0x110 stream_type 0x0F (AAC)
  字幕 PID 0x130 / 0x138
  PCR  PID 0x1FF
  ECM  PID 0x901 / CA system ID 0x0005
```

変換後のサービスは `service_type` が 4K 用の 0xAD ではなく通常の 0x01 なので、
**NID 以外に 4K を判別する手がかりはない**。

`BandType::from_nid` は元から 0x000B / 0x000C を `FourK` にしていたが、
チャンネル列挙が使う `classify_nid` には 4K の分岐がなく、地デジの NID 範囲
(0x7800–0x7FF0) 外なので `Terrestrial(Unknown(11))` に落ちていた。分類器が
2つあって食い違っていた状態。現在は両方 4K を返す。

**空間の並びは 地デジ → BS → CS → BS4K で、BS4K は必ず末尾。**
空間インデックスはクライアントの `.ch2` / `ChSet5` のアドレスそのものなので、
途中に挿すと以降が全部ずれる。末尾なら 4K のない環境のインデックスは変わらない。

> **注意**: 4K チャンネルを既にスキャン済みだった環境では、これまで 4K が
> "Unknown" 空間として地デジの並びの中にソートされており、BS/CS のインデックスが
> ずれていた。その環境では `.ch2` / `ChSet5` の再生成が必要。

### B25 (ARIB STD-B25) の無効化

変換器は ACAS を復号済みにするが、**PMT には CA 記述子が残る**。しかもその
CA system ID は 0x0005 で、これは我々の B-CAS シム
(`b25-sys/src/bindings/ffi.rs`) が名乗る ID と完全に一致する。libaribb25 は
`find_ca_descriptor_pid` で一致する ECM PID を掴むため、4K のストリームでも
B25 が起動してしまう。

実際には ECM パケットは流れていない (PID 別集計に 0x901 が現れない) ので、
復号済みの現状では素通りする。ただし `strip: true` で動かしているため、
「スクランブル済みと判定されて映像パケットが削除される」経路が一歩手前にある。

対応:

- **帯域が 4K のチャンネルでは B25 を自動で外す。** 判定は `tuner/acquire.rs`
  で行う。全選局経路が通る唯一の絞り込み点で、DB ハンドルを持っているのも
  ここだけ
- **`bon_drivers.disable_b25` で手動指定もできる** (migration 019)。4K 以外の
  「既に復号済みのソース」はストリームから判別できないため
- **未スキャンで帯域が分からないチャンネルは B25 有効のまま。** 不要な復号は
  無駄なだけだが、必要な復号を外すと映像が出ない

手動指定は現状 SQL で行う (Web API / ダッシュボードには未露出):

```sql
UPDATE bon_drivers SET disable_b25 = 1 WHERE dll_path = '...BonDriver_dantto4k.dll';
```

## 動作するもの (実機の PID 別集計で確認)

変換後の TS には SI が一式揃っている。

| PID | 内容 | 影響する機能 |
|---|---|---|
| 0x0000 | PAT | 選局・サービス解決 |
| 0x0010 | NIT | スキャン (physical_ch / remote_control_key) |
| 0x0011 | SDT | スキャン (サービス名・service_type) |
| 0x0012 | H-EIT | **番組表 (EPG) が動く** |
| 0x0014 | TOT | 時刻 |
| 0x0024 | BIT | — |
| 0x0029 | CDT | **ロゴ収集が動く** |

## 未対応 / 要確認

- **プレビュー / エンコードプロファイル**: 映像は H.265 (stream_type 0x24)。
  既定の `preview-h264` プロファイルは `--interlace tff --vpp-deinterlace normal`
  を渡すが、**BS4K は 2160/59.94p のプログレッシブ**なのでこの指定は誤り。
  4K 用プロファイル (デインタレースなし + スケール指定) の追加が必要
- **`disable_b25` の Web API / ダッシュボード露出**
- **Drop カウント**: 実機の PID 別集計では全 PID に少量の Drop が出ていた
  (映像 794484 パケット中 57)。変換器が PSI を再生成する際に連続性カウンタが
  飛んでいる可能性がある。ダッシュボードの品質表示に常時 Drop が出るなら、
  4K では計上の仕方を見直す必要がある
- **Mirakurun 互換 API**: `FourK → "BS"` にマッピング済み
  (`web/mirakurun.rs`)。EPGStation 側が 4K サービスをどう扱うかは未検証。
  dantto4k の README には PT4K + Mirakurun 運用時のタイムアウト問題への言及が
  あり、本プロジェクトの Mirakurun 互換 API でも該当する可能性がある
- **Linux**: `BonDriver_dantto4k.dll` は Windows 専用。Linux では dantto4k の
  CLI (`dantto4k - -`) をパイプ段に挟む形になる。既存の tsreadex/tsreplace
  パイプライン機構が流用できるが、未実装

## なぜソースを流用しないのか

- dantto4k は**アプリケーション**であってライブラリではない
  (`dllmain.cpp` / `bonTuner.cpp`)。ライブラリ境界を切り出す作業が要る
- ビルドに tsduck・OpenSSL・PC/SC を引き込む
- ライセンスは Apache-2.0。本リポジトリ root は GPLv3 なので取り込み自体は
  可能だが、各クレートが宣言している `MIT OR Apache-2.0` の MIT 側を潰す
- BonDriver ラッパーをそのまま使えば proxy 側の変更はほぼゼロで済む

Rust での再実装は MMT/TLV 逆多重化・MPU/MFU 再構成・MMT-SI→PSI/SI 変換・
ACAS (AES)・ARIB B24 変換を全部書くことになり、得るものに対して割に合わない。

# Vue全体移行 実装ステータス（2026-07-14）

## ソース実装済み

- Vue 3 + Vite + TypeScript + PiniaのSPA
- 概要、BonDriver、チャンネル、クライアント設定、スキャン履歴、セッション履歴、アラート、設定の全8タブ
- Bearer認証対応APIクライアント、認証対応SSE、30秒フォールバックポーリング
- BonDriver CRUD・手動スキャン
- チャンネル検索、全列ソート、表示列設定保存、編集モード、物理割当編集、一括更新・削除、CSV入出力
- アラートルール操作、クライアント切断、優先度・排他制御上書き
- `ResizeObserver` 対応メトリクスグラフ
- mpegts.jsプレビュー（npm依存をViteへ同梱、CDN非依存）
- 390px / 700px / 1100pxレスポンシブ、カード型モバイル表、44px操作領域、ダークテーマ
- focus-visible、スキップリンク、reduced-motion、モバイル全画面ダイアログ
- `rust-embed` によるVue成果物のシングルバイナリ埋め込み
- 旧HTML/CSS/JSダッシュボードの削除。Vue成果物欠落時は503で明示
- Prettier、ESLint、Stylelint設定
- CIのVue build/typecheck/lint/formatチェック
- Playwrightによる390px / 768px / 1280px・全8タブのレスポンシブ回帰テストとスクリーンショット保存
- 3画面幅の移行ベースラインを `docs/assets/dashboard-baseline-*.png` に保存
- Web API一覧を `docs/WEB_API_REFERENCE.md` に整理
- `docs/out/` 生成物、未使用 `metrics.rs`、未使用 `MuxKey` を削除

## サーバーリファクタリング

`session.rs` から以下を分離・共通化した。

- TSバックプレッシャーとRECORD昇格: `session_backpressure.rs`
- tsreplace / prefill / SID解決: `session_runtime.rs`
- 仮想空間・列挙キャッシュ: `session_space_cache.rs`
- グループ候補選択: `session_driver_selection.rs`
- 容量・退避ポリシー: `session_capacity.rs`
- チューナー引き継ぎ: `session_tuner_handoff.rs`
- NID/TSID候補集約: `session_channel_candidates.rs`

選択済みパス更新、失敗Ack、孤立チューナ削除、SetChannelSpaceの排他制御・同一チャンネル再利用・容量制御・新規開始、SelectLogicalChannel候補試行も専用helperへ分割した。fallback時は実際に選ばれたドライバー固有のspace/channelを最終処理へ渡す。

## 実行環境ブロッカー

この環境ではnpmレジストリのDNS解決とRustツールチェーンを利用できないため、以下だけ未実行。

- `npm install` / `npm run build` / `vue-tsc`
- `cargo check` / `cargo test`
- 実サーバー・実API・実チューナーを接続したE2E

npmとRustを利用できるCI向け工程は `.github/workflows/build.yml` に実装済み。CIはフロント品質検査、Vueビルド、3画面幅のPlaywright検査、Rustビルド、成果物アップロードを行う。

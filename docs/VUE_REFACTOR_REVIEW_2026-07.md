# Web GUI リファクタリング実装レビュー（2026-07）

## 結論

Vue 3 + Vite + TypeScriptへの移行はソース実装まで完了した。中間段階で使用していた `static/dashboard.html`、`dashboard.css`、`dashboard.js` は削除し、`rust-embed` で `static/vue/` の成果物だけを配信する。

## 完了した改善

- 全8タブをVueコンポーネント化
- APIクライアントとPinia storeへ状態・通信処理を集約
- チャンネル画面に検索、全列ソート、表示列保存、編集モード、物理割当編集、一括更新・削除、CSV入出力を実装
- 390px / 700px / 1100pxのレスポンシブ規則、カード型モバイル表、44px操作領域を実装
- `ResizeObserver` によるメトリクスグラフ追従を実装
- focus-visible、スキップリンク、reduced-motion、ダークテーマを実装
- `mpegts.js` をViteバンドルへ同梱し、CDN依存を撤廃
- Prettier、ESLint、Stylelint設定とCI品質ジョブを追加
- Playwrightで390px / 768px / 1280pxの全8タブを検査するレスポンシブ回帰テストを追加

## 配信構成

1. `web-ui` で `npm run build`
2. Viteが `recisdb-proxy/static/vue/` へ出力
3. Rustコンパイル時に `VueAssets` が成果物を埋め込む
4. `GET /` が `index.html`、`GET /static/vue/*path` がJS/CSS等を返す

Vue成果物がない状態で旧UIへ黙ってフォールバックする挙動は廃止した。成果物欠落時は503を返し、リリース工程の設定漏れを明示する。

## 環境依存の検証

この作業環境では外部npm DNSとRustツールチェーンを利用できないため、実バンドル・Rustコンパイル・実チューナーE2Eは未実行。CIには必要なbuild/typecheck/lint/format/Playwright工程を定義済み。

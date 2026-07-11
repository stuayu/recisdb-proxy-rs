//! Web dashboard HTML and UI.

use axum::{
    extract::State,
    http::StatusCode,
    response::Html,
};
use std::sync::Arc;
use crate::web::state::WebState;

/// Serve the main dashboard page.
pub async fn index(
    State(_web_state): State<Arc<WebState>>,
) -> Result<Html<String>, StatusCode> {
    Ok(Html(HTML_CONTENT.to_string()))
}

const HTML_CONTENT: &str = r#"
<!DOCTYPE html>
<html lang="ja">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>recisdb-proxy ダッシュボード</title>
    <link rel="preconnect" href="https://fonts.googleapis.com">
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
    <link href="https://fonts.googleapis.com/css2?family=Noto+Sans+JP:wght@400;500;700&display=swap" rel="stylesheet">
    <script>
        (function() { var t = localStorage.getItem('dashboardTheme'); if (t === 'modern') document.documentElement.classList.add('theme-modern'); })();
    </script>
    <!--
        mpegts.js (STREAMING_DESIGN.md §6.4): prefer a local copy so the
        preview player has no CDN dependency. Operators should place the
        library at recisdb-proxy/static/mpegts.js (served by GET /static/mpegts.js,
        unauthenticated like /logos/:file — see web/api.rs::get_static_asset).
        If that 404s, fall back to a CDN build so preview still works out of
        the box. This environment could not fetch/vendor the ~200KB minified
        file itself, so only this loader shim is shipped, not the library.
    -->
    <script src="/static/mpegts.js" onerror="
        (function() {
            console.warn('recisdb-proxy: /static/mpegts.js not found locally, falling back to CDN. ' +
                         'Place a local copy at recisdb-proxy/static/mpegts.js to avoid this.');
            var s = document.createElement('script');
            s.src = 'https://cdn.jsdelivr.net/npm/mpegts.js@1.7.3/dist/mpegts.min.js';
            document.head.appendChild(s);
        })();
    "></script>
    <style>
        * { margin: 0; padding: 0; box-sizing: border-box; }

        body {
            font-family: "Meiryo UI", "Meiryo", "MS UI Gothic", "メイリオ", "ＭＳ ゴシック", sans-serif;
            background: #888888;
            min-height: 100vh;
            color: #2d2d2d;
            font-size: 13px;
        }

        .container { width: 100%; max-width: 100%; box-shadow: none; display: flex; flex-direction: column; min-height: 100vh; }

        header {
            background: #1a3c6e;
            padding: 0 12px;
            display: flex;
            justify-content: space-between;
            align-items: center;
            min-height: 46px;
            border-bottom: 3px solid #e8a000;
            flex-shrink: 0;
        }

        h1 { color: #ffffff; font-size: 16px; font-weight: bold; letter-spacing: 0.5px; }
        .subtitle { color: #a8c0d8; font-size: 11px; margin-top: 1px; }

        /* Main layout: sidebar + content */
        .main-layout {
            display: flex;
            flex: 1;
            min-height: 0;
        }
        .tabs-body {
            flex: 1;
            min-width: 0;
            display: flex;
            flex-direction: column;
        }

        /* Vertical Tab Navigation */
        .tabs {
            display: flex;
            flex-direction: column;
            background: #1e3d6e;
            width: 130px;
            flex-shrink: 0;
            border-right: 2px solid #0d2244;
        }

        .tab {
            padding: 10px 12px;
            cursor: pointer;
            color: #b8cce4;
            font-weight: normal;
            border: none;
            background: transparent;
            font-size: 12px;
            font-family: inherit;
            border-bottom: 1px solid #0d2244;
            text-align: left;
            white-space: nowrap;
            overflow: hidden;
            text-overflow: ellipsis;
        }

        .tab:hover { background: #2a5298; color: #ffffff; }
        .tab.active {
            background: #f0f0f0;
            color: #1a3c6e;
            font-weight: bold;
            border-left: 3px solid #e8a000;
            padding-left: 9px;
        }

        /* Tab Content */
        .tab-content { display: none; background: #f0f0f0; padding: 10px; flex: 1; overflow-x: auto; overflow-y: auto; border: none; }
        .tab-content.active { display: block; }

        /* Stats Grid */
        .stats-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 0; margin-bottom: 10px; border: 1px solid #999999; }
        .stat-card { background: #ffffff; padding: 8px 12px; text-align: center; border-right: 1px solid #cccccc; }
        .stat-card:last-child { border-right: none; }
        .stat-label { color: #4a6080; font-size: 11px; font-weight: bold; margin-bottom: 2px; padding-bottom: 2px; border-bottom: 1px solid #d8dde5; }
        .stat-value { color: #1a3c6e; font-size: 24px; font-weight: bold; margin-top: 2px; }

        /* Tables */
        table { width: 100%; border-collapse: collapse; border: 1px solid #999999; }
        th { background: #1a3c6e; padding: 7px 10px; text-align: left; font-weight: bold; color: #ffffff; border: 1px solid #2a5298; font-size: 12px; white-space: nowrap; }
        td { padding: 6px 10px; border: 1px solid #cccccc; color: #2d2d2d; font-size: 12px; vertical-align: middle; }
        tr:nth-child(even) td { background: #f0f4f8; }
        tr:nth-child(odd) td { background: #ffffff; }
        tr:hover td { background: #dce8f8 !important; }
        code { background: #ebebeb; border: 1px solid #cccccc; padding: 1px 4px; border-radius: 0; font-size: 11px; font-family: "Courier New", Courier, monospace; }

        /* Desktop/Tablet: keep table usable with horizontal scroll */
        /* Column visibility for #clients-table is managed entirely by JS column picker */
        .responsive-table { min-width: 720px; }
        #clients-table { min-width: 1700px; }

        /* Performance graphs */
        .performance-graphs { display: flex; gap: 10px; flex-wrap: wrap; margin-top: 10px; }
        .graph-container { background: #ffffff; border: 1px solid #cccccc; padding: 10px 12px; flex: 1; min-width: 220px; }
        .graph-container h4 { font-size: 12px; color: #1a3c6e; font-weight: bold; margin-bottom: 5px; padding-bottom: 4px; border-bottom: 1px solid #dddddd; }
        .sparkline { width: 100%; height: 70px; }

        /* Buttons */
        .btn { display: inline-block; padding: 5px 12px; border: 1px solid; border-radius: 0; cursor: pointer; font-size: 12px; font-family: inherit; text-decoration: none; }
        .btn-primary { background: #2a5298; color: #ffffff; border-color: #1a3c6e; }
        .btn-primary:hover { background: #1a3c6e; }
        .btn-secondary { background: #e0e0e0; color: #333333; border-color: #999999; }
        .btn-secondary:hover { background: #cccccc; }
        .btn-success { background: #2d7d32; color: #ffffff; border-color: #1b5e20; }
        .btn-success:hover { background: #1b5e20; }
        .btn-danger { background: #c62828; color: #ffffff; border-color: #8b0000; }
        .btn-danger:hover { background: #8b0000; }
        .btn-warning { background: #e65100; color: #ffffff; border-color: #bf360c; }
        .btn-warning:hover { background: #bf360c; }
        .btn-sm { padding: 3px 7px; font-size: 11px; }

        /* Status Badges */
        .badge { display: inline-block; padding: 2px 8px; font-size: 11px; font-weight: bold; border: 1px solid; }
        .badge-success { background: #e8f5e9; color: #1b5e20; border-color: #4caf50; }
        .badge-danger { background: #ffebee; color: #b71c1c; border-color: #e53935; }
        .badge-warning { background: #fff3e0; color: #e65100; border-color: #ff9800; }
        .badge-info { background: #e3f2fd; color: #0d47a1; border-color: #42a5f5; }

        .channel-logo {
            width: 22px;
            height: 22px;
            object-fit: contain;
            vertical-align: middle;
            margin-right: 5px;
            border: 1px solid #dddddd;
            background: #ffffff;
        }

        /* Modal */
        .modal { display: none; position: fixed; z-index: 1000; left: 0; top: 0; width: 100%; height: 100%; background: rgba(0,0,0,0.55); }
        .modal.active { display: flex; align-items: center; justify-content: center; }
        .modal-content {
            background: white;
            border: 2px solid #1a3c6e;
            box-shadow: 5px 5px 20px rgba(0,0,0,0.45);
            max-width: 550px;
            width: 90%;
            max-height: 80vh;
            overflow-y: auto;
            padding: 0 20px 20px;
        }
        .modal h3 {
            background: #1a3c6e;
            color: #ffffff;
            font-size: 14px;
            font-weight: bold;
            padding: 10px 20px;
            margin: 0 -20px 15px;
            border-bottom: 3px solid #e8a000;
        }

        /* Form Elements */
        .form-group { margin-bottom: 13px; }
        .form-group label { display: block; color: #2d2d2d; margin-bottom: 3px; font-weight: bold; font-size: 12px; }
        .form-group input, .form-group select { width: 100%; padding: 5px 8px; border: 1px solid #aaaaaa; border-radius: 0; font-size: 12px; font-family: inherit; background: #ffffff; }
        .form-group input:focus, .form-group select:focus { border-color: #2a5298; outline: 1px solid #2a5298; box-shadow: none; }
        .form-group input[readonly] { background: #f0f0f0; color: #666666; }
        .form-group small { display: block; color: #666666; font-size: 11px; margin-top: 3px; }
        .form-check { display: flex; align-items: center; gap: 6px; }
        .form-check input[type="checkbox"] { width: auto; }

        .settings-form { max-width: 600px; }
        .settings-form .form-group { margin-bottom: 16px; }

        .form-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 15px; padding-top: 12px; border-top: 1px solid #cccccc; }

        /* Section Header */
        .section-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 8px; padding: 3px 8px; background: #e8ecf0; border-left: 4px solid #1a3c6e; border-bottom: 1px solid #b0b8c4; }
        .section-header h3 { color: #1a3c6e; font-size: 13px; font-weight: bold; }

        /* Settings section headers (direct children of tab-content) */
        #settings > h3 {
            color: #1a3c6e;
            font-size: 13px;
            font-weight: bold;
            border-left: 4px solid #1a3c6e;
            padding: 3px 8px;
            margin: 14px 0 8px;
            background: #e8ecf0;
        }
        #settings > h3:first-child { margin-top: 0; }

        /* Empty State */
        .empty-state { text-align: center; padding: 25px; color: #888888; font-size: 12px; background: #fafafa; }

        /* Toggle Switch */
        .toggle { position: relative; display: inline-block; width: 42px; height: 22px; }
        .toggle input { opacity: 0; width: 0; height: 0; }
        .toggle-slider { position: absolute; cursor: pointer; top: 0; left: 0; right: 0; bottom: 0; background: #aaaaaa; border: 1px solid #888888; transition: 0.2s; }
        .toggle-slider:before { position: absolute; content: ""; height: 16px; width: 16px; left: 2px; bottom: 2px; background: white; transition: 0.2s; }
        .toggle input:checked + .toggle-slider { background: #2a5298; border-color: #1a3c6e; }
        .toggle input:checked + .toggle-slider:before { transform: translateX(20px); }

        /* Filter Bar */
        .filter-bar { display: flex; gap: 8px; margin-bottom: 12px; flex-wrap: wrap; align-items: center; }
        .filter-bar select, .filter-bar input { padding: 5px 8px; border: 1px solid #aaaaaa; border-radius: 0; font-size: 12px; font-family: inherit; }

        .column-picker {
            margin: 6px 0 10px;
            padding: 8px 12px;
            background: #f0f0f0;
            border: 1px solid #cccccc;
        }
        .column-picker summary {
            cursor: pointer;
            font-size: 12px;
            color: #333333;
            font-weight: bold;
        }
        .column-picker-grid {
            margin-top: 8px;
            display: grid;
            grid-template-columns: repeat(auto-fit, minmax(130px, 1fr));
            gap: 6px 10px;
        }
        .column-picker-grid label {
            font-size: 12px;
            color: #333333;
            display: flex;
            align-items: center;
            gap: 5px;
        }

        /* Loading */
        .loading { text-align: center; padding: 20px; color: #666666; font-size: 12px; }

        /* Sortable headers */
        th.sortable { cursor: pointer; user-select: none; position: relative; padding-right: 20px; }
        th.sortable:hover { background: #2a5298; }
        th.sortable::after { content: '⇅'; position: absolute; right: 5px; opacity: 0.5; font-size: 10px; }
        th.sortable.asc::after { content: '▲'; opacity: 1; }
        th.sortable.desc::after { content: '▼'; opacity: 1; }

        .sort-bar { display: flex; gap: 8px; align-items: center; margin: 4px 0 8px; flex-wrap: wrap; padding: 5px 8px; background: #eaeaea; border: 1px solid #cccccc; }
        .sort-bar label { color: #555555; font-size: 12px; font-weight: bold; }
        .mobile-only { display: none; }

        /* Channel inline edit mode */
        .channel-edit-controls { display: none; gap: 8px; align-items: center; }
        .channel-edit-controls.active { display: flex; }
        .channel-view-controls { display: flex; gap: 8px; align-items: center; }
        .channel-view-controls.hidden { display: none; }
        tr.ch-edit-row td input[type="text"],
        tr.ch-edit-row td input[type="number"] {
            width: 100%; padding: 3px 5px; border: 1px solid #aaaaaa; border-radius: 0;
            font-size: 11px; font-family: inherit; box-sizing: border-box;
        }
        tr.ch-edit-row td input[type="number"].priority-input { width: 58px; }
        tr.ch-edit-row.ch-new-row td { background: #eef4ff !important; }
        tr.ch-edit-row.ch-modified-row td { background: #fffbea !important; }
        tr.ch-edit-row.ch-deleted-row { opacity: 0.45; }
        tr.ch-edit-row.ch-deleted-row td { text-decoration: line-through; }
        .ch-new-ids { display: flex; gap: 3px; align-items: center; flex-wrap: wrap; }
        .ch-new-ids input { width: 62px !important; }
        .ch-new-ids label { font-size: 11px; color: #555555; }
        #ch-edit-save-msg { font-size: 12px; }

        @media (max-width: 768px) {
            .stats-grid { grid-template-columns: repeat(2, 1fr); }
            .main-layout { flex-direction: column; }
            .tabs {
                flex-direction: row;
                width: 100%;
                border-right: none;
                border-bottom: 2px solid #0d2244;
                flex-wrap: wrap;
                overflow-x: auto;
            }
            .tab {
                flex: 1;
                min-width: 64px;
                text-align: center;
                padding: 8px 6px;
                font-size: 11px;
                border-bottom: none;
                border-right: 1px solid #0d2244;
            }
            .tab.active {
                border-left: none;
                padding-left: 6px;
                border-top: 2px solid #e8a000;
                border-bottom: none;
            }
            .tab-content { padding: 8px; }
            h1 { font-size: 15px; }

            .mobile-only { display: flex; }

            .responsive-table thead { display: none; }
            .responsive-table, .responsive-table tbody, .responsive-table tr, .responsive-table td { display: block; width: 100%; }
            .responsive-table tr { background: #ffffff !important; border: 1px solid #cccccc; border-radius: 0; margin-bottom: 6px; overflow: hidden; }
            .responsive-table td { display: flex; justify-content: space-between; align-items: flex-start; gap: 8px; padding: 6px 10px; border-bottom: 1px solid #eeeeee; text-align: right; flex-wrap: wrap; background: transparent !important; }
            .responsive-table td::before { content: attr(data-label); flex: 0 0 45%; color: #555555; font-size: 11px; font-weight: bold; text-align: left; }
            .responsive-table td:last-child { border-bottom: none; }

            #clients-table { min-width: 100%; }
        }

        /* ================================================================
           テーマ切替ボタン（ヘッダー内固定）
           ================================================================ */
        #theme-toggle-btn {
            background: transparent;
            color: rgba(255,255,255,0.85);
            border: 1px solid rgba(255,255,255,0.38);
            padding: 4px 12px;
            cursor: pointer;
            font-size: 11px;
            font-family: inherit;
            white-space: nowrap;
            border-radius: 2px;
        }
        #theme-toggle-btn:hover { background: rgba(255,255,255,0.16); color: #ffffff; }

        /* ================================================================
           モダンテーマ（NEC スタイル / Noto Sans JP）
           <html class="theme-modern"> で有効化
           ================================================================ */
        html.theme-modern body {
            font-family: "Noto Sans JP", "Noto Sans", "Hiragino Kaku Gothic ProN", "Yu Gothic UI", sans-serif;
            background: #e8edf4;
        }
        html.theme-modern header { background: #003087; border-bottom-color: #0082c8; }
        html.theme-modern .subtitle { color: #80acd0; }
        html.theme-modern .tabs { background: #002060; border-right-color: #001540; }
        html.theme-modern .tab { color: #90b8d8; border-bottom-color: #001540; }
        html.theme-modern .tab:hover { background: #0050a0; color: #ffffff; }
        html.theme-modern .tab.active { background: #f4f6f9; color: #003087; border-left-color: #0082c8; }
        html.theme-modern .tab-content { background: #f4f6f9; padding: 14px; }
        html.theme-modern .stats-grid { border-color: #c8d8ec; }
        html.theme-modern .stat-card { border-right-color: #c8d8ec; }
        html.theme-modern .stat-label { color: #4a6890; border-bottom-color: #c8d8ec; }
        html.theme-modern .stat-value { color: #003087; }
        html.theme-modern table { border-color: #b8cce0; }
        html.theme-modern th { background: #003087; border-color: #0050a0; font-weight: 500; }
        html.theme-modern td { border-color: #d0dcea; }
        html.theme-modern tr:nth-child(even) td { background: #eef4fb; }
        html.theme-modern tr:hover td { background: #d8eaf8 !important; }
        html.theme-modern code { background: #e8edf6; border-color: #b8cce0; }
        html.theme-modern .btn { border-radius: 4px; font-weight: 500; }
        html.theme-modern .btn-primary { background: #0050a0; border-color: #0050a0; }
        html.theme-modern .btn-primary:hover { background: #003087; border-color: #003087; }
        html.theme-modern .btn-secondary { background: #f0f4f8; color: #003087; border-color: #a8c0d8; }
        html.theme-modern .btn-secondary:hover { background: #dce8f4; }
        html.theme-modern .btn-success { background: #27ae60; border-color: #27ae60; }
        html.theme-modern .btn-success:hover { background: #1e8449; border-color: #1e8449; }
        html.theme-modern .btn-danger { background: #c0392b; border-color: #c0392b; }
        html.theme-modern .btn-danger:hover { background: #a93226; border-color: #a93226; }
        html.theme-modern .btn-warning { background: #e67e22; border-color: #e67e22; }
        html.theme-modern .btn-warning:hover { background: #ca6f1e; border-color: #ca6f1e; }
        html.theme-modern .badge { border-radius: 3px; }
        html.theme-modern .badge-success { background: #e8f8f0; color: #1a7a40; border-color: #4caf70; }
        html.theme-modern .badge-info    { background: #e0f0fa; color: #0050a0; border-color: #60b0e0; }
        html.theme-modern .badge-warning { background: #fef4e8; color: #c06000; border-color: #f0a030; }
        html.theme-modern .badge-danger  { background: #fde8e8; color: #a02020; border-color: #e06060; }
        html.theme-modern .section-header { background: #e4ecf8; border-left-color: #0050a0; border-bottom-color: #a8c0d8; }
        html.theme-modern .section-header h3 { color: #003087; }
        html.theme-modern #settings > h3 { background: #e4ecf8; border-left-color: #0050a0; color: #003087; }
        html.theme-modern th.sortable:hover { background: #0050a0; }
        html.theme-modern .modal-content { border-color: #003087; border-radius: 6px; overflow: hidden; }
        html.theme-modern .modal h3 { background: #003087; border-bottom-color: #0082c8; }
        html.theme-modern .form-group input, html.theme-modern .form-group select { border-radius: 3px; border-color: #a8bcd4; }
        html.theme-modern .form-group input:focus, html.theme-modern .form-group select:focus { border-color: #0050a0; outline-color: #0050a0; }
        html.theme-modern .toggle input:checked + .toggle-slider { background: #0050a0; border-color: #003087; }
        html.theme-modern .sort-bar { background: #e4ecf8; border-color: #b8cce0; }
        html.theme-modern .column-picker { background: #e8edf6; border-color: #b8cce0; }
        html.theme-modern .column-picker summary { color: #003087; }
        html.theme-modern .empty-state { background: #f8fafc; }
        html.theme-modern .graph-container { border-color: #b8cce0; }
        html.theme-modern .graph-container h4 { color: #003087; border-bottom-color: #c8d8ec; }
        html.theme-modern .filter-bar select, html.theme-modern .filter-bar input { border-radius: 3px; border-color: #a8bcd4; }
        html.theme-modern .form-actions { border-top-color: #c8d8ec; }
        html.theme-modern #theme-toggle-btn { border-color: rgba(255,255,255,0.38); }
        @media (max-width: 768px) {
            html.theme-modern .tab.active { border-top-color: #0082c8; border-left: none; padding-left: 6px; }
        }
    </style>
</head>
<body>
    <div class="container">
        <header>
            <div>
                <h1>recisdb-proxy</h1>
                <p class="subtitle">TVプロキシサーバー 監視・管理コンソール</p>
            </div>
            <div style="display:flex;align-items:center;gap:10px;">
                <div id="connection-status">
                    <span class="badge badge-success">接続中</span>
                </div>
                <button id="theme-toggle-btn" onclick="toggleTheme()" title="デザインテーマの切り替え">モダン</button>
            </div>
        </header>

        <div class="main-layout">
        <nav class="tabs">
            <button class="tab active" data-tab="overview">概要</button>
            <button class="tab" data-tab="bondrivers">BonDriver</button>
            <button class="tab" data-tab="channels">チャンネル</button>
            <button class="tab" data-tab="client-guide">クライアント設定</button>
            <button class="tab" data-tab="scan-history">スキャン履歴</button>
            <button class="tab" data-tab="session-history">セッション履歴</button>
            <button class="tab" data-tab="alerts">アラート</button>
            <button class="tab" data-tab="settings">設定</button>
        </nav>
        <div class="tabs-body">

        <!-- Overview Tab -->
        <div id="overview" class="tab-content active">
            <div class="stats-grid">
                <div class="stat-card">
                    <div class="stat-label">アクティブチューナー</div>
                    <div class="stat-value" id="stat-active-tuners">-</div>
                </div>
                <div class="stat-card">
                    <div class="stat-label">接続クライアント</div>
                    <div class="stat-value" id="stat-clients">-</div>
                </div>
                <div class="stat-card">
                    <div class="stat-label">総セッション</div>
                    <div class="stat-value" id="stat-sessions">-</div>
                </div>
                <div class="stat-card">
                    <div class="stat-label">登録チャンネル</div>
                    <div class="stat-value" id="stat-channels">-</div>
                </div>
            </div>

            <div class="section-header">
                <h3>接続中のクライアント</h3>
                <button class="btn btn-secondary btn-sm" onclick="refreshClients()">更新</button>
            </div>
            <details class="column-picker" id="clients-column-picker-wrap">
                <summary>表示列を調整</summary>
                <div class="column-picker-grid" id="clients-column-picker"></div>
            </details>
            <table id="clients-table" class="responsive-table sortable-table">
                <thead>
                    <tr>
                        <th class="sortable" data-sort-type="number">セッションID</th>
                        <th class="sortable" data-sort-type="text">クライアント</th>
                        <th class="sortable" data-sort-type="text">ホスト名</th>
                        <th class="sortable" data-sort-type="text">状態</th>
                        <th class="sortable" data-sort-type="text">選択チューナー</th>
                        <th class="sortable" data-sort-type="text">チャンネル</th>
                        <th class="sortable" data-sort-type="number">信号レベル</th>
                        <th class="sortable" data-sort-type="number">送信パケット</th>
                        <th class="sortable" data-sort-type="number">Drop</th>
                        <th class="sortable" data-sort-type="number">Scramble</th>
                        <th class="sortable" data-sort-type="number">Error</th>
                        <th class="sortable" data-sort-type="number">ビットレート</th>
                        <th class="sortable" data-sort-type="number">優先度</th>
                        <th class="sortable" data-sort-type="text">排他</th>
                        <th class="sortable" data-sort-type="text">上書き</th>
                        <th class="sortable" data-sort-type="text">クラス</th>
                        <th class="sortable" data-sort-type="text">プリフィル</th>
                        <th>操作</th>
                    </tr>
                </thead>
                <tbody id="clients-body">
                    <tr><td colspan="18" class="empty-state">接続中のクライアントはありません</td></tr>
                </tbody>
            </table>
            <div id="client-metrics-panel" style="margin-top: 16px; display: none;">
                <div class="section-header" style="margin-bottom: 8px;">
                    <h3>クライアント詳細</h3>
                    <span id="client-metrics-title" style="color:#666;font-size:12px;"></span>
                </div>
                <div class="performance-graphs">
                    <div class="graph-container">
                        <h4>ビットレート (Mbps)</h4>
                        <svg id="bitrate-graph" class="sparkline"></svg>
                    </div>
                    <div class="graph-container">
                        <h4>パケットロス率 (%)</h4>
                        <svg id="packet-loss-graph" class="sparkline"></svg>
                    </div>
                    <div class="graph-container">
                        <h4>信号レベル (dB)</h4>
                        <svg id="signal-graph" class="sparkline"></svg>
                    </div>
                </div>
                <div class="performance-graphs" style="margin-top: 10px;">
                    <div class="graph-container" style="min-width: 260px;">
                        <h4>配信経路の損失 (プロキシ内部)</h4>
                        <table style="font-size:12px;">
                            <tbody>
                                <tr><td>broadcast lag (chunks)</td><td id="loss-broadcast-lag-chunks" style="text-align:right;">-</td></tr>
                                <tr><td>TS queue drop (chunks)</td><td id="loss-ts-queue-chunks" style="text-align:right;">-</td></tr>
                                <tr><td>encoder stall (events)</td><td id="loss-encoder-stall-events" style="text-align:right;">-</td></tr>
                            </tbody>
                        </table>
                        <p style="font-size:11px;color:#888;margin-top:4px;">受信段階(電波/上流)由来のCCエラーはここには含まれず、Drop とロス上位PIDに計上されます。</p>
                    </div>
                    <div class="graph-container" style="min-width: 260px;">
                        <h4>ロス上位 PID (CC error)</h4>
                        <table style="font-size:12px;">
                            <thead><tr><th>PID</th><th>種別</th><th>CC errors</th></tr></thead>
                            <tbody id="top-loss-pids-body"><tr><td colspan="3" class="empty-state">データなし</td></tr></tbody>
                        </table>
                    </div>
                </div>
            </div>
        </div>

        <!-- BonDriver Tab -->
        <div id="bondrivers" class="tab-content">
            <div class="section-header">
                <h3>BonDriver 一覧</h3>
                <div style="display:flex; gap:8px;">
                    <button class="btn btn-primary btn-sm" onclick="openCreateBonDriver()">追加</button>
                    <button class="btn btn-secondary btn-sm" onclick="refreshBonDrivers()">更新</button>
                </div>
            </div>
            <table id="bondrivers-table" class="responsive-table sortable-table">
                <thead>
                    <tr>
                        <th class="sortable" data-sort-type="text">DLLパス</th>
                        <th class="sortable" data-sort-type="text">表示名</th>
                        <th class="sortable" data-sort-type="text">グループ名</th>
                        <th class="sortable" data-sort-type="number">品質スコア</th>
                        <th class="sortable" data-sort-type="number">Drop率</th>
                        <th class="sortable" data-sort-type="number">総セッション</th>
                        <th class="sortable" data-sort-type="number">最大インスタンス</th>
                        <th class="sortable" data-sort-type="text">自動スキャン</th>
                        <th class="sortable" data-sort-type="datetime">次回スキャン</th>
                        <th>操作</th>
                    </tr>
                </thead>
                <tbody id="bondrivers-body">
                    <tr><td colspan="10" class="loading">読み込み中...</td></tr>
                </tbody>
            </table>
        </div>

        <!-- Channels Tab -->
        <div id="channels" class="tab-content">
            <div class="section-header">
                <h3>チャンネル一覧</h3>
                <div style="display: flex; gap: 10px; flex-wrap: wrap; align-items: center;">
                    <!-- 通常モードのコントロール -->
                    <div class="channel-view-controls" id="channel-view-controls">
                        <select id="channel-bondriver-filter" onchange="refreshChannels()">
                            <option value="">すべてのBonDriver</option>
                        </select>
                        <label class="form-check" style="font-size: 13px;">
                            <input type="checkbox" id="channel-group-filter" onchange="refreshChannels()" checked>
                            論理チャンネル
                        </label>
                        <label class="form-check" style="font-size: 13px;">
                            <input type="checkbox" id="channel-enabled-filter" onchange="refreshChannels()">
                            有効のみ
                        </label>
                        <button class="btn btn-secondary btn-sm" onclick="refreshChannels()">更新</button>
                        <button class="btn btn-warning btn-sm" onclick="enterChannelEditMode()">編集モード</button>
                        <a id="channel-export-btn" class="btn btn-secondary btn-sm" href="/api/channels/export" download="channels.csv">CSVエクスポート</a>
                        <label class="btn btn-secondary btn-sm" style="cursor:pointer;margin:0;">
                            CSVインポート
                            <input type="file" id="channel-import-input" accept=".csv,text/csv" style="display:none" onchange="onChannelImport(this)">
                        </label>
                    </div>
                    <!-- 編集モードのコントロール -->
                    <div class="channel-edit-controls" id="channel-edit-controls">
                        <span id="ch-edit-save-msg"></span>
                        <button class="btn btn-success btn-sm" onclick="addChannelRow()">＋ 行を追加</button>
                        <button class="btn btn-primary btn-sm" onclick="saveChannelEdits()">保存</button>
                        <button class="btn btn-secondary btn-sm" onclick="exitChannelEditMode()">キャンセル</button>
                    </div>
                </div>
            </div>
            <div class="sort-bar mobile-only">
                <label for="channel-sort-key-1">並び替え</label>
                <select id="channel-sort-key-1" onchange="setChannelSortFromUI()">
                    <option value="is_enabled">有効</option>
                    <option value="id">ID</option>
                    <option value="channel_name">チャンネル名</option>
                    <option value="raw_name">raw名</option>
                    <option value="nid">NID/SID/TSID</option>
                    <option value="sid">SID</option>
                    <option value="tsid">TSID</option>
                    <option value="manual_sheet">枝番</option>
                    <option value="band_type">バンド</option>
                    <option value="terrestrial_region">地域</option>
                    <option value="network_name">ネットワーク</option>
                    <option value="physical_ch">物理CH</option>
                    <option value="remote_control_key">リモコン</option>
                    <option value="service_type">サービス種別</option>
                    <option value="tuner_count">チューナー</option>
                    <option value="bon_driver_path">BonDriver</option>
                    <option value="bon_space">BonSpace</option>
                    <option value="bon_channel">BonChannel</option>
                    <option value="priority">優先度</option>
                    <option value="failure_count">失敗回数</option>
                    <option value="scan_time">スキャン日時</option>
                    <option value="last_seen">最終確認</option>
                    <option value="created_at">登録日時</option>
                    <option value="updated_at">更新日時</option>
                </select>

                <select id="channel-sort-key-2" onchange="setChannelSortFromUI()">
                    <option value="">（第2キーなし）</option>
                    <option value="is_enabled">有効</option>
                    <option value="id">ID</option>
                    <option value="channel_name">チャンネル名</option>
                    <option value="raw_name">raw名</option>
                    <option value="nid">NID/SID/TSID</option>
                    <option value="sid">SID</option>
                    <option value="tsid">TSID</option>
                    <option value="manual_sheet">枝番</option>
                    <option value="band_type">バンド</option>
                    <option value="terrestrial_region">地域</option>
                    <option value="network_name">ネットワーク</option>
                    <option value="physical_ch">物理CH</option>
                    <option value="remote_control_key">リモコン</option>
                    <option value="service_type">サービス種別</option>
                    <option value="tuner_count">チューナー</option>
                    <option value="bon_driver_path">BonDriver</option>
                    <option value="bon_space">BonSpace</option>
                    <option value="bon_channel">BonChannel</option>
                    <option value="priority">優先度</option>
                    <option value="failure_count">失敗回数</option>
                    <option value="scan_time">スキャン日時</option>
                    <option value="last_seen">最終確認</option>
                    <option value="created_at">登録日時</option>
                    <option value="updated_at">更新日時</option>
                </select>

                <select id="channel-sort-key-3" onchange="setChannelSortFromUI()">
                    <option value="">（第3キーなし）</option>
                    <option value="is_enabled">有効</option>
                    <option value="id">ID</option>
                    <option value="channel_name">チャンネル名</option>
                    <option value="raw_name">raw名</option>
                    <option value="nid">NID/SID/TSID</option>
                    <option value="sid">SID</option>
                    <option value="tsid">TSID</option>
                    <option value="manual_sheet">枝番</option>
                    <option value="band_type">バンド</option>
                    <option value="terrestrial_region">地域</option>
                    <option value="network_name">ネットワーク</option>
                    <option value="physical_ch">物理CH</option>
                    <option value="remote_control_key">リモコン</option>
                    <option value="service_type">サービス種別</option>
                    <option value="tuner_count">チューナー</option>
                    <option value="bon_driver_path">BonDriver</option>
                    <option value="bon_space">BonSpace</option>
                    <option value="bon_channel">BonChannel</option>
                    <option value="priority">優先度</option>
                    <option value="failure_count">失敗回数</option>
                    <option value="scan_time">スキャン日時</option>
                    <option value="last_seen">最終確認</option>
                    <option value="created_at">登録日時</option>
                    <option value="updated_at">更新日時</option>
                </select>

                <button class="btn btn-secondary btn-sm" id="channel-sort-order-1" onclick="toggleChannelSortOrder(0)">第1:昇順</button>
                <button class="btn btn-secondary btn-sm" id="channel-sort-order-2" onclick="toggleChannelSortOrder(1)">第2:昇順</button>
                <button class="btn btn-secondary btn-sm" id="channel-sort-order-3" onclick="toggleChannelSortOrder(2)">第3:昇順</button>
            </div>
            <details class="column-picker" id="channels-column-picker-wrap">
                <summary>表示列を調整</summary>
                <div class="column-picker-grid" id="channels-column-picker"></div>
            </details>
            <table id="channels-table" class="responsive-table">
                <thead>
                    <tr>
                        <th class="sortable" data-sort="id">ID</th>
                        <th class="sortable" data-sort="is_enabled">有効</th>
                        <th class="sortable" data-sort="channel_name">チャンネル名</th>
                        <th class="sortable" data-sort="raw_name">raw名</th>
                        <th class="sortable" data-sort="nid">NID/SID/TSID</th>
                        <th class="sortable" data-sort="manual_sheet">枝番</th>
                        <th class="sortable" data-sort="band_type">バンド</th>
                        <th class="sortable" data-sort="terrestrial_region">地域</th>
                        <th class="sortable" data-sort="network_name">ネットワーク</th>
                        <th class="sortable" data-sort="physical_ch">物理CH</th>
                        <th class="sortable" data-sort="remote_control_key">リモコン</th>
                        <th class="sortable" data-sort="service_type">サービス種別</th>
                        <th class="sortable" data-sort="tuner_count">チューナー</th>
                        <th class="sortable" data-sort="bon_driver_path">BonDriver</th>
                        <th class="sortable" data-sort="bon_space">BonSpace</th>
                        <th class="sortable" data-sort="bon_channel">BonChannel</th>
                        <th class="sortable" data-sort="priority">優先度</th>
                        <th class="sortable" data-sort="failure_count">失敗回数</th>
                        <th class="sortable" data-sort="scan_time">スキャン日時</th>
                        <th class="sortable" data-sort="last_seen">最終確認</th>
                        <th class="sortable" data-sort="created_at">登録日時</th>
                        <th class="sortable" data-sort="updated_at">更新日時</th>
                        <th>操作</th>
                    </tr>
                </thead>
                <tbody id="channels-body">
                    <tr><td colspan="23" class="loading">読み込み中...</td></tr>
                </tbody>
            </table>
        </div>

        <!-- Client Setup Guide Tab -->
        <div id="client-guide" class="tab-content">
            <div class="section-header">
                <h3>クライアント設定ガイド</h3>
                <button class="btn btn-secondary btn-sm" onclick="refreshClientGuide()">更新</button>
            </div>
            <p style="margin: 0 0 16px; line-height: 1.7;">
                TVTest や EDCB などのクライアントは、チャンネル一覧に表示される物理チャンネル
                (BonSpace / BonChannel) を直接指定する必要は<b>ありません</b>。
                クライアントの BonDriver_NetworkProxy はこのサーバーからチャンネル一覧を自動取得し、
                チューニング空間とチャンネルを<b>名前で</b>表示します。設定が必要なのは以下の2点だけです。
            </p>

            <h4 style="margin: 20px 0 8px;">STEP 1. 接続先チューナーを選ぶ（INI の Tuner= に指定する名前）</h4>
            <p style="margin: 0 0 10px; color: #888; font-size: 13px;">
                「グループ」を選ぶと、グループ内の空いているチューナーをサーバーが自動で選びます（推奨）。
            </p>
            <table id="client-guide-targets-table" class="responsive-table">
                <thead>
                    <tr>
                        <th>選択</th>
                        <th>Tuner= に書く名前</th>
                        <th>種類</th>
                        <th>有効チャンネル数</th>
                        <th>備考</th>
                    </tr>
                </thead>
                <tbody id="client-guide-targets-body">
                    <tr><td colspan="5" class="loading">読み込み中...</td></tr>
                </tbody>
            </table>

            <h4 style="margin: 24px 0 8px;">STEP 2. BonDriver_NetworkProxy.ini を作る</h4>
            <p style="margin: 0 0 10px; color: #888; font-size: 13px;">
                クライアントPCの BonDriver_NetworkProxy.dll と同じフォルダに、下の内容で
                BonDriver_NetworkProxy.ini を作成してください（そのままコピーできます）。
            </p>
            <pre id="client-guide-ini" style="background: rgba(128,128,128,0.12); border: 1px solid rgba(128,128,128,0.35); border-radius: 6px; padding: 12px 14px; font-size: 13px; overflow-x: auto; user-select: all;"></pre>
            <button class="btn btn-primary btn-sm" onclick="copyClientGuideIni(this)">INIの内容をコピー</button>

            <h4 style="margin: 24px 0 8px;">STEP 3. チャンネル設定ファイルをダウンロード（必要な場合のみ）</h4>
            <p style="margin: 0 0 10px; color: #888; font-size: 13px;">
                これらを置くと TVTest / EpgDataCap_Bon でのチャンネルスキャンを省略できます
                （置かない場合は各ソフトで一度スキャンを実行してください）。配置先:
                <b>.ch2</b> → BonDriver_NetworkProxy.dll と同じフォルダ /
                <b>ChSet4.txt・ChSet5.txt</b> → EDCB の Setting フォルダ。
                「まとめてダウンロード」には接続先入りの BonDriver_NetworkProxy.ini と手順の README も入ります。
                DLL をリネームして使っている場合 (例: BonDriver_NetworkProxy_MLT5.dll) は、
                .ch2 と ChSet4.txt のファイル名の先頭部分も同じ名前に変更してください
                (例: BonDriver_NetworkProxy_MLT5.ch2 /
                BonDriver_NetworkProxy_MLT5(BonDriver_NetworkProxy).ChSet4.txt)。
                地デジのリモコン番号やネットワーク名が 0 / 空になる場合は、古いスキャン結果に
                値が記録されていません。「BonDriver」タブから再スキャンすると取得されます。
            </p>
            <div style="display:flex; gap:8px; flex-wrap:wrap; margin-bottom: 8px;">
                <button class="btn btn-secondary btn-sm" onclick="downloadClientFile('tvtest-ch2')">TVTest用 .ch2</button>
                <button class="btn btn-secondary btn-sm" onclick="downloadClientFile('chset4')">EDCB用 ChSet4.txt</button>
                <button class="btn btn-secondary btn-sm" onclick="downloadClientFile('chset5')">EDCB用 ChSet5.txt</button>
                <button class="btn btn-primary btn-sm" onclick="downloadClientFile('bundle')">まとめてダウンロード (zip)</button>
            </div>
            <div id="client-guide-download-msg" style="font-size:12px; color:#888;"></div>

            <h4 style="margin: 24px 0 8px;">STEP 4. クライアントに表示されるチャンネルを確認</h4>
            <p style="margin: 0 0 10px; color: #888; font-size: 13px;">
                選択したチューナーで接続したとき、クライアントには以下の空間・チャンネルが<b>この順番・この名前で</b>表示されます。
                TVTest では一覧から名前で選ぶだけで選局できます。空間番号・CH番号は、番号での指定が必要なツールを使う場合の参考値です。
            </p>
            <div id="client-guide-view">
                <div class="empty-state">STEP 1 でチューナーを選択すると表示されます</div>
            </div>
        </div>

        <!-- Settings Tab -->
        <div id="settings" class="tab-content">
            <h3>スキャンスケジューラー設定</h3>
            <div class="settings-form">
                <div class="form-group">
                    <label for="check-interval">スケジューラーチェック間隔（秒）</label>
                    <input type="number" id="check-interval" min="1" value="60">
                    <small>スケジューラーが何秒ごとにスキャン対象をチェックするか</small>
                </div>

                <div class="form-group">
                    <label for="max-concurrent">最大並列スキャン数</label>
                    <input type="number" id="max-concurrent" min="1" value="1">
                    <small>同時に実行可能なBonDriverのスキャン数</small>
                </div>

                <div class="form-group">
                    <label for="scan-timeout">スキャンタイムアウト（秒）</label>
                    <input type="number" id="scan-timeout" min="60" value="900">
                    <small>各BonDriver単位でのスキャンタイムアウト時間</small>
                </div>

                <div class="form-group">
                    <label for="signal-lock-wait">SetChannel後の待機時間（ミリ秒）</label>
                    <input type="number" id="signal-lock-wait" min="1" value="500">
                    <small>SetChannel2応答後に信号判定/読み出しを開始するまでの待機時間</small>
                </div>

                <div class="form-group">
                    <label for="ts-read-timeout">映像データ読み出し時間（ミリ秒）</label>
                    <input type="number" id="ts-read-timeout" min="1" value="300000">
                    <small>チャンネル解析時にTSデータを読み出す最大時間</small>
                </div>

                <div style="margin-top: 20px; display: flex; gap: 10px;">
                    <button class="btn btn-primary" onclick="saveScanConfig()">保存</button>
                    <button class="btn btn-secondary" onclick="loadScanConfig()">リセット</button>
                </div>

                <div id="config-message" style="margin-top: 15px; display: none;"></div>
            </div>

            <h3 style="margin-top: 30px;">チューナ最適化設定</h3>
            <div class="settings-form">
                <div class="form-group">
                    <label for="tuner-keep-alive">Keep-Alive（秒）</label>
                    <input type="number" id="tuner-keep-alive" min="0" value="60">
                    <small>最終クライアント切断後にチューナを保持する時間</small>
                </div>

                <div class="form-group">
                    <label class="form-check">
                        <input type="checkbox" id="tuner-prewarm-enabled" checked>
                        Pre-Warm を有効にする
                    </label>
                </div>

                <div class="form-group">
                    <label for="tuner-prewarm-timeout">Pre-Warm タイムアウト（秒）</label>
                    <input type="number" id="tuner-prewarm-timeout" min="1" value="30">
                    <small>OpenTuner 後に SetChannel が来ない場合の待機時間</small>
                </div>

                <div class="form-group">
                    <label for="tuner-setch-retry-interval">SetChannel リトライ間隔（ms）</label>
                    <input type="number" id="tuner-setch-retry-interval" min="1" value="500">
                    <small>SetChannel 失敗時の再試行間隔（ネットワーク遅延向け）</small>
                </div>

                <div class="form-group">
                    <label for="tuner-setch-retry-timeout">SetChannel リトライ上限時間（ms）</label>
                    <input type="number" id="tuner-setch-retry-timeout" min="1" value="10000">
                    <small>SetChannel 成功を待つ最大時間</small>
                </div>

                <div class="form-group">
                    <label for="tuner-signal-poll-interval">シグナル値ポーリング間隔（ms）</label>
                    <input type="number" id="tuner-signal-poll-interval" min="1" value="500">
                    <small>SetChannel 後に信号値を確認する間隔</small>
                </div>

                <div class="form-group">
                    <label for="tuner-signal-wait-timeout">シグナル待機上限時間（ms）</label>
                    <input type="number" id="tuner-signal-wait-timeout" min="1" value="10000">
                    <small>信号値が返るまで待つ最大時間</small>
                </div>

                <div class="form-group">
                    <label for="tuner-prefill-view-ms">プリフィル時間・視聴（ms）</label>
                    <input type="number" id="tuner-prefill-view-ms" min="0" value="1000">
                    <small>視聴クラスのストリーム開始時に事前に貯めるバッファ時間（0でバイパス）</small>
                </div>

                <div class="form-group">
                    <label for="tuner-prefill-preview-ms">プリフィル時間・プレビュー（ms）</label>
                    <input type="number" id="tuner-prefill-preview-ms" min="0" value="2000">
                    <small>プレビュークラスのストリーム開始時に事前に貯めるバッファ時間（0でバイパス）</small>
                </div>

                <div class="form-group">
                    <label for="tuner-prefill-record-ms">プリフィル時間・録画（ms）</label>
                    <input type="number" id="tuner-prefill-record-ms" min="0" value="6000">
                    <small>録画クラスのストリーム開始時に事前に貯めるバッファ時間（0でバイパス）</small>
                </div>

                <div class="form-group">
                    <label for="tuner-jitter-safety-factor">ジッタ安全係数</label>
                    <input type="number" id="tuner-jitter-safety-factor" min="0.1" step="0.1" value="1.5">
                    <small>プリフィルバッファサイズ = ビットレート × プリフィル時間 × この係数</small>
                </div>

                <div style="margin-top: 20px; display: flex; gap: 10px;">
                    <button class="btn btn-primary" onclick="saveTunerConfig()">保存</button>
                    <button class="btn btn-secondary" onclick="loadTunerConfig()">リセット</button>
                </div>

                <div id="tuner-config-message" style="margin-top: 15px; display: none;"></div>
            </div>

            <h3 style="margin-top: 30px;">外部エンコード（BNDPセッション用 / tsreplace）設定</h3>
            <p style="font-size:12px;color:#666;">
                BonDriver経由（TVTest等）の視聴/録画セッション専用の設定です。
                ブラウザプレビューには一切影響しません（プレビューは下の
                「ブラウザプレビュー」セクションで設定します）。
            </p>
            <div class="settings-form">
                <div class="form-group">
                    <label class="form-check">
                        <input type="checkbox" id="tsreplace-enabled">
                        BNDPセッション（TVTest等）のエンコードを有効にする
                    </label>
                </div>

                <div class="form-group">
                    <label for="tsreplace-command-path">実行コマンド</label>
                    <input type="text" id="tsreplace-command-path" readonly disabled placeholder="例: tsreplace または /usr/local/bin/tsreplace">
                    <small>設定ファイル (recisdb-proxy.toml の [tsreplace] command_path) でのみ変更可能。セキュリティ上の理由により、この画面やAPIからは変更できません。</small>
                </div>

                <div class="form-group">
                    <label for="tsreplace-arguments">引数テンプレート</label>
                    <input type="text" id="tsreplace-arguments" placeholder="例: --preset fast --output -">
                    <small>起動時に付与する引数（空欄可）。<code>{SID}</code> は対象サービスIDに置換されます</small>
                </div>

                <div class="form-group">
                    <label for="tsreplace-preprocessor-path">前段コマンド（プリプロセッサ）</label>
                    <input type="text" id="tsreplace-preprocessor-path" readonly disabled placeholder="例: C:\DTV\tsreadex\tsreadex.exe（未設定なら単段）">
                    <small>TS → 前段 → エンコーダ の2段パイプの前段（例: tsreadex）。設定ファイル (recisdb-proxy.toml の [tsreplace] preprocessor_path) でのみ変更可能。空欄なら従来どおり単段動作</small>
                </div>

                <div class="form-group">
                    <label for="tsreplace-preprocessor-arguments">前段コマンドの引数テンプレート</label>
                    <input type="text" id="tsreplace-preprocessor-arguments" placeholder="例: -x 18 -n {SID} -">
                    <small>前段コマンドに付与する引数。<code>{SID}</code> は対象サービスIDに置換されます</small>
                </div>

                <div class="form-group">
                    <label for="tsreplace-read-timeout">読み取りタイムアウト（ms）</label>
                    <input type="number" id="tsreplace-read-timeout" min="1" value="10000">
                    <small>外部プロセス出力を待つ最大時間</small>
                </div>

                <div class="form-group">
                    <label for="tsreplace-max-encoders">同時エンコード数の上限</label>
                    <input type="number" id="tsreplace-max-encoders" min="1" value="2">
                    <small>共有エンコーダの同時起動数（HWエンコードの同時セッション数目安、BNDP/ブラウザプレビュー共通のプール上限）。上限到達時は非エンコードTSで配信</small>
                </div>

                <div class="form-group">
                    <label class="form-check">
                        <input type="checkbox" id="tsreplace-passthrough-on-error" checked>
                        エラー時は非エンコードTSでフォールバック
                    </label>
                </div>

                <div style="margin-top: 20px; display: flex; gap: 10px;">
                    <button class="btn btn-primary" onclick="saveTsreplaceConfig()">保存</button>
                    <button class="btn btn-secondary" onclick="loadTsreplaceConfig()">リセット</button>
                </div>

                <div id="tsreplace-config-message" style="margin-top: 15px; display: none;"></div>
            </div>

            <h3 style="margin-top: 30px;">ブラウザプレビュー（?profile=preview）設定</h3>
            <p style="font-size:12px;color:#666;">
                ダッシュボードのプレビュー再生専用の設定です。上のBNDP用（tsreplace）
                設定とは完全に独立しています。エンコード引数自体は下の
                エンコードプロファイル（purpose=preview）から取られます。
            </p>
            <div class="settings-form">
                <div class="form-group">
                    <label class="form-check">
                        <input type="checkbox" id="preview-enabled">
                        ブラウザプレビューのエンコードを有効にする
                    </label>
                </div>

                <div class="form-group">
                    <label for="preview-command-path">エンコーダ実行コマンド</label>
                    <input type="text" id="preview-command-path" readonly disabled placeholder="例: C:\DTV\KonomiTV\server\thirdparty\QSVEncC\QSVEncC.exe">
                    <small>設定ファイル (recisdb-proxy.toml の [preview] command_path) でのみ変更可能。セキュリティ上の理由により、この画面やAPIからは変更できません</small>
                </div>

                <div class="form-group">
                    <label for="preview-preprocessor-path">前段コマンド（プリプロセッサ）</label>
                    <input type="text" id="preview-preprocessor-path" readonly disabled placeholder="例: C:\DTV\KonomiTV\server\thirdparty\tsreadex\tsreadex.exe（未設定なら単段）">
                    <small>TS → 前段 → エンコーダ の2段パイプの前段（例: tsreadex）。設定ファイル (recisdb-proxy.toml の [preview] preprocessor_path) でのみ変更可能</small>
                </div>

                <div class="form-group">
                    <label for="preview-preprocessor-arguments">前段コマンドの引数テンプレート</label>
                    <input type="text" id="preview-preprocessor-arguments" placeholder="例: -x 18/38/39 -n {SID} -a 13 -b 5 -c 1 -u 1 -d 13 -">
                    <small>前段コマンドに付与する引数。<code>{SID}</code> は対象サービスIDに置換されます。初期値は tsreadex の推奨設定（サービス選択・音声/字幕ストリーム補完・字幕のID3変換）です</small>
                </div>

                <div class="form-group">
                    <label for="preview-read-timeout">読み取りタイムアウト（ms）</label>
                    <input type="number" id="preview-read-timeout" min="1" value="10000">
                    <small>エンコーダ出力を待つ最大時間（超過でチェーンを強制終了）</small>
                </div>

                <div style="margin-top: 20px; display: flex; gap: 10px;">
                    <button class="btn btn-primary" onclick="savePreviewConfig()">保存</button>
                    <button class="btn btn-secondary" onclick="loadPreviewConfig()">リセット</button>
                </div>

                <div id="preview-config-message" style="margin-top: 15px; display: none;"></div>
            </div>

            <h3 style="margin-top: 30px;">エンコードプロファイル (STREAMING_DESIGN.md §5.3)</h3>
            <p style="font-size:12px;color:#666;">
                録画・プレビュー用途ごとのコーデック/ビットレート/追加引数の組み合わせ。
                実行コマンド本体はTOML設定でのみ変更可能で、BNDPセッションは
                [tsreplace] command_path、ブラウザプレビューは [preview] command_path
                が使われます。ブラウザプレビューは <code>purpose=preview</code> の
                最初の有効な行の追加引数を使用します。
            </p>
            <div class="section-header">
                <span></span>
                <button class="btn btn-secondary btn-sm" onclick="refreshEncodeProfiles()">更新</button>
            </div>
            <table id="encode-profiles-table" class="responsive-table">
                <thead>
                    <tr>
                        <th>有効</th>
                        <th>名前</th>
                        <th>用途</th>
                        <th>コーデック</th>
                        <th>コンテナ</th>
                        <th>ビットレート</th>
                        <th>追加引数</th>
                        <th>操作</th>
                    </tr>
                </thead>
                <tbody id="encode-profiles-body">
                    <tr><td colspan="8" class="loading">読み込み中...</td></tr>
                </tbody>
            </table>
            <div style="margin-top:10px;">
                <button class="btn btn-primary btn-sm" onclick="openCreateEncodeProfile()">プロファイル追加</button>
            </div>
        </div>

        <!-- History Tab -->
        <div id="scan-history" class="tab-content">
            <div class="section-header">
                <h3>スキャン履歴</h3>
                <button class="btn btn-secondary btn-sm" onclick="refreshHistory()">更新</button>
            </div>
            <table id="history-table" class="responsive-table sortable-table">
                <thead>
                    <tr>
                        <th class="sortable" data-sort-type="datetime">日時</th>
                        <th class="sortable" data-sort-type="number">BonDriver ID</th>
                        <th class="sortable" data-sort-type="text">結果</th>
                        <th class="sortable" data-sort-type="number">チャンネル数</th>
                        <th class="sortable" data-sort-type="text">メッセージ</th>
                    </tr>
                </thead>
                <tbody id="history-body">
                    <tr><td colspan="5" class="loading">読み込み中...</td></tr>
                </tbody>
            </table>
        </div>

        <!-- Session History Tab -->
        <div id="session-history" class="tab-content">
            <div class="section-header">
                <h3>セッション履歴</h3>
                <div class="filter-bar">
                    <input type="text" id="session-filter-address" placeholder="クライアントアドレスで絞り込み">
                    <button class="btn btn-secondary btn-sm" onclick="refreshSessionHistory()">更新</button>
                </div>
            </div>
            <table id="session-history-table" class="responsive-table sortable-table">
                <thead>
                    <tr>
                        <th class="sortable" data-sort-type="datetime">開始</th>
                        <th class="sortable" data-sort-type="datetime">終了</th>
                        <th class="sortable" data-sort-type="text">クライアント</th>
                        <th class="sortable" data-sort-type="text">チャンネル</th>
                        <th class="sortable" data-sort-type="number">時間</th>
                        <th class="sortable" data-sort-type="number">送信パケット</th>
                        <th class="sortable" data-sort-type="number">Drop</th>
                        <th class="sortable" data-sort-type="number">Scramble</th>
                        <th class="sortable" data-sort-type="number">Error</th>
                        <th class="sortable" data-sort-type="number">平均ビットレート</th>
                    </tr>
                </thead>
                <tbody id="session-history-body">
                    <tr><td colspan="10" class="empty-state">セッション履歴がありません</td></tr>
                </tbody>
            </table>
        </div>

        <!-- Alerts Tab -->
        <div id="alerts" class="tab-content">
            <div class="section-header">
                <h3>アクティブアラート</h3>
                <button class="btn btn-secondary btn-sm" onclick="refreshAlerts()">更新</button>
            </div>
            <table id="alerts-table" class="responsive-table sortable-table">
                <thead>
                    <tr>
                        <th class="sortable" data-sort-type="datetime">発生時刻</th>
                        <th class="sortable" data-sort-type="number">ルールID</th>
                        <th class="sortable" data-sort-type="number">セッション</th>
                        <th class="sortable" data-sort-type="text">メッセージ</th>
                        <th>操作</th>
                    </tr>
                </thead>
                <tbody id="alerts-body">
                    <tr><td colspan="5" class="empty-state">アクティブアラートはありません</td></tr>
                </tbody>
            </table>

            <div class="section-header" style="margin-top: 20px;">
                <h3>アラートルール</h3>
                <button class="btn btn-primary btn-sm" onclick="openModal('alert-rule-modal')">ルール追加</button>
            </div>
            <table id="alert-rules-table" class="responsive-table sortable-table">
                <thead>
                    <tr>
                        <th class="sortable" data-sort-type="number">ID</th>
                        <th class="sortable" data-sort-type="text">名前</th>
                        <th class="sortable" data-sort-type="text">監視項目</th>
                        <th class="sortable" data-sort-type="text">条件（比較）</th>
                        <th class="sortable" data-sort-type="number">しきい値</th>
                        <th class="sortable" data-sort-type="text">有効</th>
                        <th>操作</th>
                    </tr>
                </thead>
                <tbody id="alert-rules-body">
                    <tr><td colspan="7" class="empty-state">ルールがありません</td></tr>
                </tbody>
            </table>
        </div>

    <!-- BonDriver Edit Modal -->
    <div class="modal" id="bondriver-modal">
        <div class="modal-content">
            <h3>BonDriver 設定編集</h3>
            <form id="bondriver-form">
                <input type="hidden" id="bd-id">
                <div class="form-group">
                    <label>DLLパス</label>
                    <input type="text" id="bd-path" placeholder="例: BonDriver_PX-W3U4.dll" required>
                </div>
                <div class="form-group">
                    <label>表示名</label>
                    <input type="text" id="bd-name" placeholder="表示名を入力">
                </div>
                <div class="form-group">
                    <label>グループ名</label>
                    <input type="text" id="bd-group-name" placeholder="例：PX-MLT, PX-S">
                </div>
                <div class="form-group">
                    <label>最大インスタンス数</label>
                    <input type="number" id="bd-max-instances" min="1" max="32" value="1">
                </div>
                <div class="form-group">
                    <label class="form-check">
                        <input type="checkbox" id="bd-auto-scan">
                        自動スキャンを有効にする
                    </label>
                </div>
                <div class="form-group">
                    <label>スキャン間隔（時間）</label>
                    <input type="number" id="bd-scan-interval" min="1" max="720" value="24">
                </div>
                <div class="form-group">
                    <label>スキャン優先度</label>
                    <input type="number" id="bd-scan-priority" min="0" max="100" value="0">
                </div>
                <div class="form-group">
                    <label class="form-check">
                        <input type="checkbox" id="bd-passive-scan">
                        パッシブスキャンを有効にする
                    </label>
                </div>
                <div class="form-actions">
                    <button type="button" class="btn btn-secondary" onclick="closeModal('bondriver-modal')">キャンセル</button>
                    <button type="submit" class="btn btn-primary">保存</button>
                </div>
            </form>
        </div>
    </div>
    
        <div id="alert-rule-modal" class="modal">
            <div class="modal-content">
                <h3>アラートルール追加</h3>
                <form id="alert-rule-form">
                    <div class="form-group">
                        <label>名前</label>
                        <input type="text" id="ar-name" required>
                        <small>例: Drop率が高いときに通知</small>
                    </div>
                    <div class="form-group">
                        <label>監視項目</label>
                        <select id="ar-metric">
                            <option value="drop_rate">Drop率</option>
                            <option value="scramble_rate">Scramble率</option>
                            <option value="error_rate">Error率</option>
                            <option value="signal_level">信号レベル</option>
                            <option value="bitrate">ビットレート</option>
                        </select>
                        <small>数値の監視項目を選びます（文字列の一致/部分一致はありません）</small>
                    </div>
                    <div class="form-group">
                        <label>条件（比較）</label>
                        <select id="ar-condition">
                            <option value="gt">より大きい (>)</option>
                            <option value="gte">以上 (>=)</option>
                            <option value="lt">より小さい (<)</option>
                            <option value="lte">以下 (<=)</option>
                        </select>
                        <small>例: Drop率 が 0.05 以上 なら通知</small>
                    </div>
                    <div class="form-group">
                        <label>しきい値</label>
                        <input type="number" id="ar-threshold" step="0.01" required>
                        <small>数値を入力（例: 0.05, 15, 2800）</small>
                    </div>
                    <div class="form-group">
                        <label>Webhook URL（任意）</label>
                        <input type="text" id="ar-webhook-url" placeholder="https://...">
                        <small>Discord/Slack/LINE などの Webhook URL</small>
                    </div>
                    <div class="form-group">
                        <label>Webhook 形式</label>
                        <select id="ar-webhook-format">
                            <option value="generic">汎用（JSON）</option>
                            <option value="discord">Discord</option>
                            <option value="slack">Slack</option>
                            <option value="line">LINE</option>
                        </select>
                        <small>送信先に合わせて選択します</small>
                    </div>
                    <div class="form-group">
                        <label class="form-check">
                            <input type="checkbox" id="ar-enabled" checked>
                            有効にする
                        </label>
                    </div>
                    <div class="form-actions">
                        <button type="button" class="btn btn-secondary" onclick="closeModal('alert-rule-modal')">キャンセル</button>
                        <button type="submit" class="btn btn-primary">保存</button>
                    </div>
                </form>
            </div>
        </div>

    <!-- Channel CSV Import Result Modal -->
    <div class="modal" id="channel-import-modal">
        <div class="modal-content" style="max-width:420px;">
            <h3>CSVインポート結果</h3>
            <div id="channel-import-result" style="margin-bottom:15px;"></div>
            <div class="form-actions">
                <button type="button" class="btn btn-primary" onclick="closeModal('channel-import-modal')">閉じる</button>
            </div>
        </div>
    </div>

    <!-- Channel Edit Modal -->
    <div class="modal" id="channel-modal">
        <div class="modal-content">
            <h3>チャンネル設定編集</h3>
            <form id="channel-form">
                <input type="hidden" id="ch-id">
                <div class="form-group">
                    <label>チャンネル情報</label>
                    <input type="text" id="ch-info" readonly>
                </div>
                <div class="form-group">
                    <label>チャンネル名</label>
                    <input type="text" id="ch-name" placeholder="チャンネル名を入力">
                </div>
                <div class="form-group">
                    <label>優先度</label>
                    <input type="number" id="ch-priority" min="-100" max="100" value="0">
                </div>
                <div class="form-group">
                    <label class="form-check">
                        <input type="checkbox" id="ch-enabled" checked>
                        有効にする
                    </label>
                </div>
                <div class="form-actions">
                    <button type="button" class="btn btn-danger" onclick="deleteChannel()" style="margin-right: auto;">削除</button>
                    <button type="button" class="btn btn-secondary" onclick="closeModal('channel-modal')">キャンセル</button>
                    <button type="submit" class="btn btn-primary">保存</button>
                </div>
            </form>
        </div>
    </div>

    <!-- Channel Preview Modal (STREAMING_DESIGN.md §6.3/§6.4) -->
    <div class="modal" id="channel-preview-modal">
        <div class="modal-content" style="max-width:720px;">
            <h3 id="preview-title">プレビュー</h3>
            <p style="font-size:12px;color:#666;margin-top:-8px;">
                地上波などMPEG-2の放送はブラウザで直接再生できないため、常に「preview」プロファイル
                (共有エンコーダによるH.264変換、STREAMING_DESIGN.md §6.2) 経由で再生します。
                エンコーダが未設定/無効/上限到達の場合は再生できません（設定タブの外部エンコード設定を確認してください）。
            </p>
            <video id="preview-video" controls autoplay muted style="width:100%;background:#000;max-height:405px;"></video>
            <div id="preview-status" style="margin-top:10px;font-size:12px;color:#666;"></div>
            <div class="form-actions">
                <button type="button" class="btn btn-secondary" onclick="closeChannelPreview()">閉じる</button>
            </div>
        </div>
    </div>

    <!-- Encode Profile Edit Modal (STREAMING_DESIGN.md §5.3) -->
    <div class="modal" id="encode-profile-modal">
        <div class="modal-content">
            <h3 id="encode-profile-modal-title">エンコードプロファイル</h3>
            <form id="encode-profile-form">
                <input type="hidden" id="ep-id">
                <div class="form-group">
                    <label>名前</label>
                    <input type="text" id="ep-name" placeholder="例: preview-h264" required>
                </div>
                <div class="form-group">
                    <label>用途 (purpose)</label>
                    <select id="ep-purpose">
                        <option value="preview">preview（ブラウザプレビュー）</option>
                        <option value="record">record（録画）</option>
                        <option value="view">view（視聴・予約）</option>
                    </select>
                </div>
                <div class="form-group">
                    <label>コーデック</label>
                    <select id="ep-codec">
                        <option value="h264">h264</option>
                        <option value="hevc">hevc</option>
                    </select>
                    <small>preview用途はブラウザ互換性のためH.264を推奨します (STREAMING_DESIGN.md §6.2)</small>
                </div>
                <div class="form-group">
                    <label>コンテナ</label>
                    <input type="text" id="ep-container" value="mpegts">
                </div>
                <div class="form-group">
                    <label>目標ビットレート (bps、空欄可)</label>
                    <input type="number" id="ep-bitrate" min="0" placeholder="例: 2000000">
                </div>
                <div class="form-group">
                    <label>追加引数</label>
                    <input type="text" id="ep-extra-args" placeholder="tsreplace/QSVEncCへ渡す引数">
                    <small>実行コマンド本体（command_path）は外部エンコード設定側のみで変更可能です</small>
                </div>
                <div class="form-group">
                    <label class="form-check">
                        <input type="checkbox" id="ep-enabled" checked>
                        有効にする
                    </label>
                </div>
                <div class="form-actions">
                    <button type="button" class="btn btn-danger" id="ep-delete-btn" onclick="deleteEncodeProfile()" style="margin-right: auto; display:none;">削除</button>
                    <button type="button" class="btn btn-secondary" onclick="closeModal('encode-profile-modal')">キャンセル</button>
                    <button type="submit" class="btn btn-primary">保存</button>
                </div>
            </form>
        </div>
    </div>

    <div id="client-override-modal" class="modal">
        <div class="modal-content">
            <h3>クライアント制御の上書き</h3>
            <form id="client-override-form">
                <input type="hidden" id="override-session-id">
                <div class="form-group">
                    <label>優先度</label>
                    <input type="number" id="override-priority" placeholder="未設定は空欄">
                    <label class="form-check" style="margin-top:6px;">
                        <input type="checkbox" id="override-priority-enabled">
                        優先度を上書きする
                    </label>
                </div>
                <div class="form-group">
                    <label class="form-check">
                        <input type="checkbox" id="override-exclusive">
                        排他ロックを強制
                    </label>
                    <label class="form-check" style="margin-top:6px;">
                        <input type="checkbox" id="override-exclusive-enabled">
                        排他を上書きする
                    </label>
                </div>
                <div class="form-actions">
                    <button type="button" class="btn btn-secondary" onclick="closeModal('client-override-modal')">キャンセル</button>
                    <button type="submit" class="btn btn-primary">保存</button>
                </div>
            </form>
        </div>
    </div>

    <script>
        // ---------------------------------------------------------------
        // Web API auth token handling (REVIEW_2026-07.md S2).
        //
        // The dashboard shell (this HTML) is served without authentication,
        // but every /api/* route requires `Authorization: Bearer <token>`.
        // The token is generated once on the server at startup and printed
        // to the server log; the user pastes it in here once, and it is
        // cached in localStorage so it survives page reloads. `window.fetch`
        // is wrapped (once, below) so every existing `fetch('/api/...')`
        // call site in this file gets the header automatically — no need to
        // touch each call individually.
        // ---------------------------------------------------------------
        const AUTH_TOKEN_STORAGE_KEY = 'recisdbProxyAuthToken';

        function getStoredAuthToken() {
            return localStorage.getItem(AUTH_TOKEN_STORAGE_KEY) || '';
        }

        function setStoredAuthToken(token) {
            if (token) {
                localStorage.setItem(AUTH_TOKEN_STORAGE_KEY, token);
            } else {
                localStorage.removeItem(AUTH_TOKEN_STORAGE_KEY);
            }
        }

        // Guards against multiple concurrent fetches each popping their own
        // prompt() on first load (several /api/* calls fire in parallel).
        let _authPromptInFlight = null;

        function requestAuthTokenFromUser(message) {
            if (_authPromptInFlight) return _authPromptInFlight;
            _authPromptInFlight = Promise.resolve().then(() => {
                const input = window.prompt(message);
                const token = (input || '').trim();
                if (token) setStoredAuthToken(token);
                _authPromptInFlight = null;
                return token;
            });
            return _authPromptInFlight;
        }

        const _nativeFetch = window.fetch.bind(window);
        window.fetch = async function authenticatedFetch(input, init) {
            const url = typeof input === 'string' ? input : (input && input.url) || '';
            const isApiCall = url.startsWith('/api/');
            init = init || {};

            // No preemptive prompt here: the client cannot know whether the
            // server has auth enabled ([web] auth_enabled = false skips the
            // check entirely), so a stored token is attached if present and
            // the user is only asked after the server actually answers 401.
            if (isApiCall) {
                const token = getStoredAuthToken();
                if (token) {
                    init = Object.assign({}, init, {
                        headers: Object.assign({}, init.headers, { 'Authorization': 'Bearer ' + token }),
                    });
                }
            }

            let res = await _nativeFetch(input, init);

            if (isApiCall && res.status === 401) {
                setStoredAuthToken(''); // stale/wrong token, drop it
                const token = await requestAuthTokenFromUser(
                    'recisdb-proxy: APIトークンを入力してください\n(サーバー起動時のログに表示されます)'
                );
                if (token) {
                    init = Object.assign({}, init, {
                        headers: Object.assign({}, init.headers, { 'Authorization': 'Bearer ' + token }),
                    });
                    res = await _nativeFetch(input, init);
                }
            }

            return res;
        };

        // Tab switching
        document.querySelectorAll('.tab').forEach(tab => {
            tab.addEventListener('click', () => {
                document.querySelectorAll('.tab').forEach(t => t.classList.remove('active'));
                document.querySelectorAll('.tab-content').forEach(c => c.classList.remove('active'));
                tab.classList.add('active');
                document.getElementById(tab.dataset.tab).classList.add('active');

                // Load data for the tab
                if (tab.dataset.tab === 'bondrivers') refreshBonDrivers();
                else if (tab.dataset.tab === 'channels') refreshChannels();
                else if (tab.dataset.tab === 'client-guide') refreshClientGuide();
                else if (tab.dataset.tab === 'scan-history') refreshHistory();
                else if (tab.dataset.tab === 'session-history') refreshSessionHistory();
                else if (tab.dataset.tab === 'alerts') { refreshAlerts(); refreshAlertRules(); }
            });
        });

        // Utility functions
        function formatDuration(seconds) {
            if (!seconds) return '-';
            if (seconds < 60) return `${seconds}秒`;
            if (seconds < 3600) return `${Math.floor(seconds / 60)}分`;
            return `${Math.floor(seconds / 3600)}時間${Math.floor((seconds % 3600) / 60)}分`;
        }

        function formatPackets(count) {
            if (!count) return '-';
            if (count < 1000) return count.toString();
            if (count < 1000000) return (count / 1000).toFixed(1) + 'K';
            return (count / 1000000).toFixed(1) + 'M';
        }

        function formatDateTime(timestamp) {
            if (!timestamp) return '-';
            return new Date(timestamp * 1000).toLocaleString('ja-JP');
        }

        function escapeHtml(str) {
            if (!str) return '';
            return str.replace(/[&<>"']/g, m => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[m]);
        }

        // Delegated click handler for row action buttons. Buttons emit
        // data-action + data-id (numeric, HTML-safe) instead of inline
        // onclick="fn(${JSON.stringify(...)})", so broadcast/EPG-derived
        // strings (driver/channel names) can never break out of the
        // attribute (stored-XSS). Objects are looked up by id from the
        // already-loaded arrays rather than serialized into the DOM.
        document.addEventListener('click', (event) => {
            const el = event.target.closest('[data-action]');
            if (!el) return;
            const action = el.dataset.action;
            const id = el.dataset.id;
            if (action === 'edit-bondriver') {
                const d = bondriverData.find(x => String(x.id) === id);
                if (d) editBonDriver(d);
            } else if (action === 'delete-bondriver') {
                const d = bondriverData.find(x => String(x.id) === id);
                if (d) deleteBonDriver(d.id, d.driver_name || d.dll_path);
            } else if (action === 'edit-channel') {
                const c = channelData.find(x => String(x.id) === id);
                if (c) editChannel(c);
            } else if (action === 'preview-channel') {
                const c = channelData.find(x => String(x.id) === id);
                if (c) openChannelPreview(c.id, c.channel_name || c.raw_name || ("CH" + c.id));
            } else if (action === 'edit-encode-profile') {
                const p = encodeProfileData.find(x => String(x.id) === id);
                if (p) openEditEncodeProfile(p);
            }
        });

        function applyResponsiveLabels(tableId) {
            const table = document.getElementById(tableId);
            if (!table) return;
            const headers = Array.from(table.querySelectorAll('thead th')).map(th => th.textContent.trim());
            table.querySelectorAll('tbody tr').forEach(tr => {
                tr.querySelectorAll('td').forEach((td, index) => {
                    if (td.hasAttribute('colspan')) return;
                    if (!td.hasAttribute('data-label')) {
                        td.setAttribute('data-label', headers[index] || '');
                    }
                });
            });
        }

        function parseSortValue(value, type) {
            if (type === 'number') {
                const num = parseFloat(String(value).replace(/[^0-9.\-]/g, ''));
                return isNaN(num) ? 0 : num;
            }
            if (type === 'datetime') {
                const num = parseInt(value, 10);
                if (!isNaN(num)) return num;
                const time = Date.parse(String(value));
                return isNaN(time) ? 0 : time;
            }
            return String(value).toLowerCase();
        }

        function compareParsedSortValues(a, b, type) {
            if (type === 'number' || type === 'datetime') {
                return (a ?? 0) - (b ?? 0);
            }
            const sa = String(a ?? '').toLowerCase();
            const sb = String(b ?? '').toLowerCase();
            return sa.localeCompare(sb, 'ja');
        }

        const tableSortStates = {};

        function normalizeTableSortRules(headers, rules) {
            const maxIndex = headers.length - 1;
            const normalized = [];
            const used = new Set();
            for (const r of rules || []) {
                const index = Number.isInteger(r?.index) ? r.index : -1;
                if (index < 0 || index > maxIndex || used.has(index)) continue;
                normalized.push({ index, asc: r.asc !== false });
                used.add(index);
                if (normalized.length >= 3) break;
            }
            return normalized;
        }

        function updateTableSortHeaderUI(headers, rules) {
            headers.forEach(h => {
                h.classList.remove('asc', 'desc');
                h.removeAttribute('title');
            });

            rules.forEach((r, i) => {
                const th = headers[r.index];
                if (!th) return;
                if (i === 0) {
                    th.classList.add(r.asc ? 'asc' : 'desc');
                }
                const dir = r.asc ? '昇順' : '降順';
                th.setAttribute('title', `第${i + 1}キー (${dir})`);
            });
        }

        // 現在のsort状態をテーブルのtbodyに適用する共通関数
        // refreshClients等でtbodyが再生成された後に呼ぶことで、ソート順を維持できる
        function sortTableRows(tableId) {
            const table = document.getElementById(tableId);
            if (!table) return;
            // 物理的なDOM順（非表示含む）で全th.sortableを取得する
            const allHeaders = Array.from(table.querySelectorAll('thead th'));
            const sortableHeaders = allHeaders.filter(th => th.classList.contains('sortable'));
            const rules = normalizeTableSortRules(sortableHeaders, tableSortStates[tableId] || []);
            if (rules.length === 0) return;
            const tbody = table.querySelector('tbody');
            if (!tbody) return;
            const rows = Array.from(tbody.querySelectorAll('tr')).filter(r =>
                !r.querySelector('.empty-state') && !r.querySelector('.loading'));
            if (rows.length < 2) return;
            // rule.index は sortableHeaders 内のインデックス
            // セルアクセスには allHeaders 内の物理位置が必要
            const sortablePhysicalIndices = sortableHeaders.map(th => allHeaders.indexOf(th));
            rows.sort((a, b) => {
                for (const rule of rules) {
                    const colType = sortableHeaders[rule.index]?.dataset.sortType || 'text';
                    const physIdx = sortablePhysicalIndices[rule.index];
                    const aCell = a.children[physIdx];
                    const bCell = b.children[physIdx];
                    const aVal = aCell?.dataset.sortValue ?? aCell?.textContent ?? '';
                    const bVal = bCell?.dataset.sortValue ?? bCell?.textContent ?? '';
                    const va = parseSortValue(aVal, colType);
                    const vb = parseSortValue(bVal, colType);
                    const cmp = compareParsedSortValues(va, vb, colType);
                    if (cmp !== 0) return rule.asc ? cmp : -cmp;
                }
                return 0;
            });
            rows.forEach(row => tbody.appendChild(row));
        }

        function enableTableSorting(tableId) {
            const table = document.getElementById(tableId);
            if (!table) return;
            const allHeaders = Array.from(table.querySelectorAll('thead th'));
            const headers = allHeaders.filter(th => th.classList.contains('sortable'));
            tableSortStates[tableId] = normalizeTableSortRules(headers, tableSortStates[tableId] || []);
            updateTableSortHeaderUI(headers, tableSortStates[tableId]);

            headers.forEach((th, index) => {
                th.addEventListener('click', (ev) => {
                    let rules = normalizeTableSortRules(headers, tableSortStates[tableId] || []);
                    const existingIdx = rules.findIndex(r => r.index === index);

                    if (ev.shiftKey) {
                        // Shift+クリック: 第2/第3キーとして追加・更新
                        if (existingIdx >= 0) {
                            rules[existingIdx].asc = !rules[existingIdx].asc;
                        } else {
                            rules.push({ index, asc: true });
                        }
                    } else {
                        // 通常クリック: 第1キーに昇格（同一第1キーなら昇降反転）
                        if (existingIdx === 0) {
                            rules[0].asc = !rules[0].asc;
                        } else {
                            let asc = true;
                            if (existingIdx > 0) {
                                asc = rules[existingIdx].asc;
                                rules.splice(existingIdx, 1);
                            }
                            rules.unshift({ index, asc });
                        }
                    }

                    rules = normalizeTableSortRules(headers, rules);
                    tableSortStates[tableId] = rules;
                    updateTableSortHeaderUI(headers, rules);
                    sortTableRows(tableId);
                });
            });
        }

        function renderOverrideBadge(c) {
            const hasOverride = (c.override_priority !== null && c.override_priority !== undefined) ||
                (c.override_exclusive !== null && c.override_exclusive !== undefined);
            if (!hasOverride) return '<span class="badge badge-info">なし</span> ';
            const parts = [];
            if (c.override_priority !== null && c.override_priority !== undefined) {
                parts.push(`P=${c.override_priority}`);
            }
            if (c.override_exclusive !== null && c.override_exclusive !== undefined) {
                parts.push(`E=${c.override_exclusive ? 'ON' : 'OFF'}`);
            }
            return `<span class="badge badge-warning">${parts.join(' ')}</span> `;
        }

        // Stream reliability class (STREAMING_DESIGN.md §2): view/record/preview
        function renderStreamClassBadge(streamClass) {
            const cls = streamClass || 'view';
            const labels = { view: '視聴', record: '録画', preview: 'プレビュー' };
            const badgeClass = cls === 'record' ? 'badge-danger' : (cls === 'preview' ? 'badge-warning' : 'badge-success');
            return `<span class="badge ${badgeClass}">${escapeHtml(labels[cls] || cls)}</span>`;
        }

        // Prefill/jitter buffer status (STREAMING_DESIGN.md §4 P3)
        function renderPrefillingBadge(prefilling) {
            return prefilling
                ? '<span class="badge badge-warning">バッファ中</span>'
                : '<span class="badge badge-success">配信中</span>';
        }

        // BandType: 0=Terrestrial, 1=BS, 2=CS, 3=4K, 4=Other, 5=CATV, 6=SKY
        function getBandTypeName(bandType) {
            const names = ['地デジ', 'BS', 'CS', 'BS4K', 'その他', 'CATV', 'SKY'];
            return bandType !== null && bandType !== undefined ? (names[bandType] || '不明') : '-';
        }

        function getBandBadgeClass(bandType) {
            const classes = ['badge-success', 'badge-info', 'badge-warning', 'badge-info', 'badge-danger', 'badge-warning', 'badge-info'];
            return bandType !== null && bandType !== undefined ? (classes[bandType] || 'badge-danger') : '';
        }

        function getChannelLogoHtml(c) {
            if (c.nid === null || c.nid === undefined || c.sid === null || c.sid === undefined) return '';
            const src = `/logos/${c.nid}_${c.sid}.png`;
            return `<img class="channel-logo" src="${src}" alt="logo" onerror="this.style.display='none'">`;
        }

        // Modal functions
        function openModal(id) { document.getElementById(id).classList.add('active'); }
        function closeModal(id) { document.getElementById(id).classList.remove('active'); }

        window.onclick = (e) => {
            document.querySelectorAll('.modal').forEach(m => {
                if (e.target === m) m.classList.remove('active');
            });
        };

        // Stats & Clients
        async function refreshStats() {
            try {
                const [statsRes, channelsRes] = await Promise.all([
                    fetch('/api/stats'),
                    fetch('/api/channels')
                ]);
                const stats = await statsRes.json();
                const channels = await channelsRes.json();

                if (stats.success && stats.stats) {
                    document.getElementById('stat-active-tuners').textContent = stats.stats.active_tuners || 0;
                    document.getElementById('stat-sessions').textContent = stats.stats.total_sessions_db || 0;
                }
                if (channels.success) {
                    document.getElementById('stat-channels').textContent = channels.count || 0;
                }
            } catch (e) { console.error('Failed to refresh stats:', e); }
        }

        async function refreshClients() {
            try {
                const res = await fetch('/api/clients');
                const data = await res.json();
                const tbody = document.getElementById('clients-body');
                document.getElementById('stat-clients').textContent = data.count || 0;

                if (!data.clients || data.clients.length === 0) {
                    tbody.innerHTML = '<tr><td colspan="18" class="empty-state">接続中のクライアントはありません</td></tr>';
                    applyResponsiveLabels('clients-table');
                    applyClientColumnVisibility();
                    return;
                }

                tbody.innerHTML = data.clients.map(c => `
                    <tr onclick="selectClient(${c.session_id})" style="cursor:pointer;">
                        <td data-sort-value="${c.session_id}">${c.session_id}</td>
                        <td data-sort-value="${escapeHtml(c.address)}">${escapeHtml(c.address)} <span style="color:#999;font-size:11px">(${formatDuration(c.connected_seconds)})</span></td>
                        <td data-sort-value="${escapeHtml(c.host || '-')}">${escapeHtml(c.host || '-')}</td>
                        <td data-sort-value="${c.is_streaming ? '1' : '0'}"><span class="badge ${c.is_streaming ? 'badge-success' : 'badge-warning'}">${c.is_streaming ? 'ストリーミング中' : '待機中'}</span></td>
                        <td data-sort-value="${escapeHtml(c.tuner_path || '-')}"><code>${escapeHtml(c.tuner_path || '-')}</code></td>
                        <td data-sort-value="${escapeHtml(c.channel_name || c.channel_info || '-')}">${getChannelLogoHtml(c)}${escapeHtml(c.channel_name || c.channel_info || '-')}</td>
                        <td data-sort-value="${c.signal_level != null ? c.signal_level : 0}">${c.signal_level != null ? c.signal_level.toFixed(1) : '-'} dB</td>
                        <td data-sort-value="${c.packets_sent || 0}">${formatPackets(c.packets_sent)}</td>
                        <td data-sort-value="${c.packets_dropped || 0}">${formatPackets(c.packets_dropped)}</td>
                        <td data-sort-value="${c.packets_scrambled || 0}">${formatPackets(c.packets_scrambled)}</td>
                        <td data-sort-value="${c.packets_error || 0}">${formatPackets(c.packets_error)}</td>
                        <td data-sort-value="${c.current_bitrate_mbps != null ? c.current_bitrate_mbps : 0}">${c.current_bitrate_mbps != null ? c.current_bitrate_mbps.toFixed(2) : '-'} Mbps</td>
                        <td data-sort-value="${c.effective_priority !== null && c.effective_priority !== undefined ? c.effective_priority : -99999}">${c.effective_priority !== null && c.effective_priority !== undefined ? c.effective_priority : '-'}</td>
                        <td data-sort-value="${c.effective_exclusive ? '1' : '0'}"><span class="badge ${c.effective_exclusive ? 'badge-danger' : 'badge-success'}">${c.effective_exclusive ? 'ON' : 'OFF'}</span></td>
                        <td data-sort-value="${(c.override_priority !== null && c.override_priority !== undefined) || (c.override_exclusive !== null && c.override_exclusive !== undefined) ? '1' : '0'}">
                            ${renderOverrideBadge(c)}
                            <button class="btn btn-primary btn-sm" onclick="event.stopPropagation(); openOverrideModal(${c.session_id}, ${c.override_priority !== null && c.override_priority !== undefined ? c.override_priority : 'null'}, ${c.override_exclusive !== null && c.override_exclusive !== undefined ? c.override_exclusive : 'null'});">設定</button>
                            <button class="btn btn-secondary btn-sm" onclick="event.stopPropagation(); clearOverride(${c.session_id});">解除</button>
                        </td>
                        <td data-sort-value="${escapeHtml(c.stream_class || 'view')}">${renderStreamClassBadge(c.stream_class)}</td>
                        <td data-sort-value="${c.prefilling ? '1' : '0'}">${renderPrefillingBadge(c.prefilling)}</td>
                        <td><button class="btn btn-danger btn-sm" onclick="event.stopPropagation(); disconnectClient(${c.session_id});">切断</button></td>
                    </tr>
                `).join('');
                applyResponsiveLabels('clients-table');
                applyClientColumnVisibility();
                sortTableRows('clients-table');
            } catch (e) { console.error('Failed to refresh clients:', e); }
        }

        let activeClientId = null;

        function selectClient(id) {
            activeClientId = id;
            document.getElementById('client-metrics-panel').style.display = 'block';
            document.getElementById('client-metrics-title').textContent = `Session ${id}`;
            updateClientMetrics();
        }

        async function disconnectClient(id) {
            if (!confirm('このセッションを切断しますか？')) return;
            try {
                const res = await fetch(`/api/client/${id}/disconnect`, { method: 'POST' });
                const data = await res.json();
                if (!data.success) alert('切断に失敗しました');
            } catch (e) { alert('切断に失敗しました: ' + e.message); }
        }

        function drawSparkline(svgId, data, color, minY, maxY) {
            const svg = document.getElementById(svgId);
            if (!svg) return;
            const width = svg.clientWidth || 300;
            const height = svg.clientHeight || 70;
            svg.setAttribute('viewBox', `0 0 ${width} ${height}`);

            if (!data || data.length === 0) {
                svg.innerHTML = '';
                return;
            }

            const values = data.map(d => d[1]);
            const minVal = minY !== null ? minY : Math.min(...values);
            const maxVal = maxY !== null ? maxY : Math.max(...values);
            const range = (maxVal - minVal) || 1;

            const points = data.map((d, i) => {
                const x = (i / Math.max(1, data.length - 1)) * width;
                const y = height - ((d[1] - minVal) / range) * height;
                return `${x},${y}`;
            }).join(' ');

            svg.innerHTML = `<polyline fill="none" stroke="${color}" stroke-width="2" points="${points}" />`;
        }

        // ARIB (ISDB) 固定PIDの日本語ラベル。固定でないPID(映像・音声・字幕・
        // PCR・ECM等)はサービスごとにPMTで決まるため一般名で表示する。
        function pidLabel(pid) {
            const fixed = {
                0x0000: 'PAT (番組構成の目次)',
                0x0001: 'CAT (EMM位置情報)',
                0x0010: 'NIT (ネットワーク情報)',
                0x0011: 'SDT/BAT (局名情報)',
                0x0012: 'EIT (番組表)',
                0x0013: 'RST',
                0x0014: 'TDT/TOT (時刻)',
                0x0017: 'DCT',
                0x001E: 'DIT',
                0x001F: 'SIT',
                0x0023: 'SDTT (ソフト更新告知)',
                0x0024: 'BIT',
                0x0025: 'NBIT/LDT',
                0x0026: 'EIT (ワンセグ)',
                0x0027: 'EIT (ワンセグ)',
                0x0029: 'CDT (局ロゴ)',
                0x1FFF: 'NULL (詰め物)',
            };
            if (fixed[pid] !== undefined) return fixed[pid];
            if (pid >= 0x1FC8 && pid <= 0x1FCF) return 'PMT (ワンセグ)';
            return '映像/音声/字幕など (PMT依存)';
        }

        async function updateClientMetrics() {
            if (!activeClientId) return;
            try {
                const res = await fetch(`/api/client/${activeClientId}/metrics-history`);
                const data = await res.json();
                if (!data.success) return;
                drawSparkline('bitrate-graph', data.bitrate, '#4CAF50', 0, null);
                drawSparkline('packet-loss-graph', data.packet_loss, '#FF5722', 0, null);
                drawSparkline('signal-graph', data.signal_level, '#2196F3', 0, null);
            } catch (e) { console.error('Failed to update metrics:', e); }

            try {
                const qres = await fetch(`/api/client/${activeClientId}/quality`);
                const qdata = await qres.json();
                if (!qdata.success) return;
                document.getElementById('loss-broadcast-lag-chunks').textContent = formatPackets(qdata.loss_broadcast_lag_chunks);
                document.getElementById('loss-ts-queue-chunks').textContent = formatPackets(qdata.loss_ts_queue_chunks);
                document.getElementById('loss-encoder-stall-events').textContent = formatPackets(qdata.loss_encoder_stall_events);

                const pids = qdata.top_loss_pids || [];
                const pidsBody = document.getElementById('top-loss-pids-body');
                pidsBody.innerHTML = pids.length === 0
                    ? '<tr><td colspan="3" class="empty-state">データなし</td></tr>'
                    : pids.map(p => `
                        <tr><td>0x${p[0].toString(16).toUpperCase().padStart(4, '0')}</td><td>${pidLabel(p[0])}</td><td style="text-align:right;">${formatPackets(p[1])}</td></tr>
                    `).join('');
            } catch (e) { console.error('Failed to update loss breakdown:', e); }
        }

        function openOverrideModal(sessionId, overridePriority, overrideExclusive) {
            document.getElementById('override-session-id').value = sessionId;
            document.getElementById('override-priority').value = overridePriority !== null ? overridePriority : '';
            document.getElementById('override-exclusive').checked = overrideExclusive === true;
            document.getElementById('override-priority-enabled').checked = overridePriority !== null;
            document.getElementById('override-exclusive-enabled').checked = overrideExclusive !== null;
            openModal('client-override-modal');
        }

        async function clearOverride(sessionId) {
            if (!confirm('上書きを解除しますか？')) return;
            try {
                const res = await fetch(`/api/client/${sessionId}/controls`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({
                        override_priority: null,
                        override_exclusive: null
                    })
                });
                const data = await res.json();
                if (data.success) refreshClients();
            } catch (e) { alert('解除に失敗しました: ' + e.message); }
        }

        document.getElementById('client-override-form').onsubmit = async (e) => {
            e.preventDefault();
            const sessionId = document.getElementById('override-session-id').value;
            const priorityValue = document.getElementById('override-priority').value;
            const priorityEnabled = document.getElementById('override-priority-enabled').checked;
            const exclusiveEnabled = document.getElementById('override-exclusive-enabled').checked;
            const overridePriority = priorityEnabled ? (priorityValue === '' ? 0 : parseInt(priorityValue, 10)) : null;
            const overrideExclusive = exclusiveEnabled ? document.getElementById('override-exclusive').checked : null;

            try {
                const res = await fetch(`/api/client/${sessionId}/controls`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({
                        override_priority: overridePriority,
                        override_exclusive: overrideExclusive
                    })
                });
                const data = await res.json();
                if (data.success) {
                    closeModal('client-override-modal');
                    refreshClients();
                } else {
                    alert('更新に失敗しました');
                }
            } catch (e) { alert('更新に失敗しました: ' + e.message); }
        };

        // BonDrivers
        let bondriverData = [];
        async function refreshBonDrivers() {
            try {
                const res = await fetch('/api/bondrivers/ranking');
                const data = await res.json();
                const tbody = document.getElementById('bondrivers-body');
                const filter = document.getElementById('channel-bondriver-filter');

                if (!data.success || !data.items) {
                    tbody.innerHTML = '<tr><td colspan="10" class="empty-state">BonDriverが登録されていません</td></tr>';
                    applyResponsiveLabels('bondrivers-table');
                    return;
                }

                const bondrivers = data.items.map(i => i.driver);
                bondriverData = bondrivers;

                // Update filter dropdown
                filter.innerHTML = '<option value="">すべてのBonDriver</option>' +
                    bondrivers.map(d => `<option value="${d.id}">${escapeHtml(d.driver_name || d.dll_path)}</option>`).join('');

                tbody.innerHTML = data.items.map(item => {
                    const d = item.driver;
                    const nextScan = d.next_scan_at ? formatDateTime(d.next_scan_at) : '-';
                    const quality = (item.quality_score * 100).toFixed(1) + '%';
                    const dropRate = (item.recent_drop_rate * 100).toFixed(2) + '%';
                    return `
                    <tr>
                        <td data-sort-value="${escapeHtml(d.dll_path)}"><code>${escapeHtml(d.dll_path)}</code></td>
                        <td data-sort-value="${escapeHtml(d.driver_name || '-')}">${escapeHtml(d.driver_name) || '-'}</td>
                        <td data-sort-value="${escapeHtml(d.group_name || '-')}">${escapeHtml(d.group_name) || '-'}</td>
                        <td data-sort-value="${item.quality_score}">${quality}</td>
                        <td data-sort-value="${item.recent_drop_rate}">${dropRate}</td>
                        <td data-sort-value="${item.total_sessions}">${item.total_sessions}</td>
                        <td data-sort-value="${d.max_instances}">${d.max_instances}</td>
                        <td data-sort-value="${d.auto_scan_enabled ? '1' : '0'}"><span class="badge ${d.auto_scan_enabled ? 'badge-success' : 'badge-danger'}">${d.auto_scan_enabled ? 'ON' : 'OFF'}</span></td>
                        <td data-sort-value="${d.next_scan_at || 0}">${nextScan}</td>
                        <td>
                            <button class="btn btn-primary btn-sm" data-action="edit-bondriver" data-id="${d.id}">編集</button>
                            <button class="btn btn-warning btn-sm" onclick="triggerScan(${d.id})">スキャン</button>
                            <button class="btn btn-danger btn-sm" data-action="delete-bondriver" data-id="${d.id}">削除</button>
                        </td>
                    </tr>
                `}).join('');
                applyResponsiveLabels('bondrivers-table');
                sortTableRows('bondrivers-table');
            } catch (e) { console.error('Failed to refresh bondrivers:', e); }
        }

        function editBonDriver(d) {
            document.querySelector('#bondriver-modal h3').textContent = 'BonDriver 設定編集';
            document.getElementById('bd-id').value = d.id;
            document.getElementById('bd-path').value = d.dll_path;
            document.getElementById('bd-name').value = d.driver_name || '';
            document.getElementById('bd-group-name').value = d.group_name || '';
            document.getElementById('bd-max-instances').value = d.max_instances;
            document.getElementById('bd-auto-scan').checked = d.auto_scan_enabled;
            document.getElementById('bd-scan-interval').value = d.scan_interval_hours;
            document.getElementById('bd-scan-priority').value = d.scan_priority;
            document.getElementById('bd-passive-scan').checked = d.passive_scan_enabled;
            openModal('bondriver-modal');
        }

        function openCreateBonDriver() {
            document.querySelector('#bondriver-modal h3').textContent = 'BonDriver 追加';
            document.getElementById('bd-id').value = '';
            document.getElementById('bd-path').value = '';
            document.getElementById('bd-name').value = '';
            document.getElementById('bd-group-name').value = '';
            document.getElementById('bd-max-instances').value = 1;
            document.getElementById('bd-auto-scan').checked = false;
            document.getElementById('bd-scan-interval').value = 24;
            document.getElementById('bd-scan-priority').value = 0;
            document.getElementById('bd-passive-scan').checked = false;
            openModal('bondriver-modal');
        }

        document.getElementById('bondriver-form').onsubmit = async (e) => {
            e.preventDefault();
            const id = document.getElementById('bd-id').value;
            const payload = {
                dll_path: document.getElementById('bd-path').value,
                driver_name: document.getElementById('bd-name').value || null,
                group_name: document.getElementById('bd-group-name').value || null,
                max_instances: parseInt(document.getElementById('bd-max-instances').value),
                auto_scan_enabled: document.getElementById('bd-auto-scan').checked,
                scan_interval_hours: parseInt(document.getElementById('bd-scan-interval').value),
                scan_priority: parseInt(document.getElementById('bd-scan-priority').value),
                passive_scan_enabled: document.getElementById('bd-passive-scan').checked
            };
            try {
                const isCreate = !id;
                const res = await fetch(isCreate ? '/api/bondriver' : `/api/bondriver/${id}`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(payload)
                });
                const data = await res.json();
                if (data.success) {
                    closeModal('bondriver-modal');
                    refreshBonDrivers();
                } else {
                    alert('エラー: ' + data.error);
                }
            } catch (e) { alert('保存に失敗しました: ' + e.message); }
        };

        async function deleteBonDriver(id, name) {
            if (!confirm(`BonDriver「${name}」を削除しますか？\n関連チャンネルとスキャン履歴も削除されます。`)) return;
            try {
                const res = await fetch(`/api/bondriver/${id}`, { method: 'DELETE' });
                const data = await res.json();
                if (data.success) {
                    refreshBonDrivers();
                    refreshChannels();
                } else {
                    alert('削除に失敗しました: ' + (data.error || 'unknown error'));
                }
            } catch (e) {
                alert('削除に失敗しました: ' + e.message);
            }
        }

        async function triggerScan(id) {
            if (!confirm('このBonDriverでスキャンを開始しますか？')) return;
            try {
                const res = await fetch(`/api/bondriver/${id}/scan`, { method: 'POST' });
                const data = await res.json();
                alert(data.success ? 'スキャンをスケジュールしました' : 'エラー: ' + data.error);
                refreshBonDrivers();
            } catch (e) { alert('スキャン開始に失敗しました: ' + e.message); }
        }

        // Channels - sorting state
        let channelData = [];
        let channelSortRules = [
            { key: 'nid', asc: true },
            { key: 'sid', asc: true },
            { key: 'tsid', asc: true },
        ];

        // Channel edit mode state
        let channelEditMode = false;
        // {id: {channel_name, priority, is_enabled, deleted}}
        let channelEdits = {};
        // [{_tempId, bon_driver_id, nid, sid, tsid, channel_name, priority, is_enabled, bon_space, bon_channel}]
        let channelNewRows = [];
        let channelNewRowCounter = 0;
        // List of BonDrivers for new row selector
        let bondriverList = [];

        // Clients table column visibility
        let clientsColumnVisibility = {};

        function loadClientColumnPrefs() {
            try {
                const raw = localStorage.getItem('clientsTableColumnVisibility');
                if (!raw) return {};
                const parsed = JSON.parse(raw);
                return parsed && typeof parsed === 'object' ? parsed : {};
            } catch (_) {
                return {};
            }
        }

        function saveClientColumnPrefs() {
            localStorage.setItem('clientsTableColumnVisibility', JSON.stringify(clientsColumnVisibility));
        }

        function applyClientColumnVisibility() {
            const table = document.getElementById('clients-table');
            if (!table) return;

            const isMobile = window.matchMedia('(max-width: 768px)').matches;
            const rows = table.querySelectorAll('tr');
            const checks = document.querySelectorAll('#clients-column-picker input[type="checkbox"][data-col]');

            checks.forEach(chk => {
                const col = parseInt(chk.dataset.col, 10);
                const visible = !!chk.checked;

                rows.forEach(row => {
                    const cell = row.children[col - 1];
                    if (!cell) return;

                    if (!visible) {
                        cell.style.display = 'none';
                        return;
                    }

                    // レスポンシブCSSで display:none が当たる列でも、GUI選択時は表示を優先する
                    if (isMobile) {
                        cell.style.display = '';
                    } else {
                        cell.style.display = 'table-cell';
                    }
                });
            });
        }

        // ビューポート幅に応じたクライアントテーブルのデフォルト列表示状態を返す
        // （CSSメディアクエリを廃止してJS側で統一管理）
        function getDefaultColumnVisibilityForClients(totalCols) {
            const w = window.innerWidth;
            // 旧CSSメディアクエリと同じ閾値・列番号を踏襲
            const hidden = new Set();
            if (w <= 1400) { [8, 9, 10, 11].forEach(c => hidden.add(c)); }
            if (w <= 1200) { [3, 12, 15].forEach(c => hidden.add(c)); }
            if (w <= 992)  { [5, 13, 14].forEach(c => hidden.add(c)); }
            const vis = {};
            for (let i = 1; i <= totalCols; i++) vis[i] = !hidden.has(i);
            return vis;
        }

        function initClientsColumnPicker() {
            const picker = document.getElementById('clients-column-picker');
            const table = document.getElementById('clients-table');
            if (!picker || !table) return;

            const headers = Array.from(table.querySelectorAll('thead th'));
            const totalCols = headers.length;
            // 保存済み設定とビューポート由来のデフォルトをマージ（保存済みが優先）
            const savedPrefs = loadClientColumnPrefs();
            const defaults = getDefaultColumnVisibilityForClients(totalCols);
            clientsColumnVisibility = Object.assign({}, defaults, savedPrefs);

            picker.innerHTML = headers.map((th, idx) => {
                const col = idx + 1;
                const label = th.textContent.trim() || `列${col}`;
                const checked = !!clientsColumnVisibility[col];
                const locked = (label === 'セッションID' || label === '操作');
                return `
                    <label>
                        <input type="checkbox" data-col="${col}" ${checked ? 'checked' : ''} ${locked ? 'disabled' : ''}>
                        ${escapeHtml(label)}
                    </label>
                `;
            }).join('');

            picker.querySelectorAll('input[type="checkbox"]').forEach(chk => {
                chk.addEventListener('change', (e) => {
                    const col = parseInt(e.target.dataset.col, 10);
                    clientsColumnVisibility[col] = !!e.target.checked;
                    saveClientColumnPrefs();
                    applyClientColumnVisibility();
                });
            });

            applyClientColumnVisibility();
        }

        // ============================================================
        // チャンネルテーブル 列表示設定（表示モード専用）
        // ============================================================
        // 列定義: 表示順・ラベル・ソートキー・デフォルト表示。
        // defaultVisible:false の列（DB由来の詳細列）は初期状態では非表示で、
        // 「表示列を調整」ピッカーから有効化できる。
        const CHANNEL_TABLE_COLUMNS = [
            { key: 'id',                 label: 'ID',            defaultVisible: false },
            { key: 'is_enabled',         label: '有効',          defaultVisible: true },
            { key: 'channel_name',       label: 'チャンネル名',  defaultVisible: true },
            { key: 'raw_name',           label: 'raw名',         defaultVisible: false },
            { key: 'nid',                label: 'NID/SID/TSID',  defaultVisible: true },
            { key: 'manual_sheet',       label: '枝番',          defaultVisible: false },
            { key: 'band_type',          label: 'バンド',        defaultVisible: true },
            { key: 'terrestrial_region', label: '地域',          defaultVisible: true },
            { key: 'network_name',       label: 'ネットワーク',  defaultVisible: true },
            { key: 'physical_ch',        label: '物理CH',        defaultVisible: false },
            { key: 'remote_control_key', label: 'リモコン',      defaultVisible: false },
            { key: 'service_type',       label: 'サービス種別',  defaultVisible: false },
            { key: 'tuner_count',        label: 'チューナー',    defaultVisible: true },
            { key: 'bon_driver_path',    label: 'BonDriver',     defaultVisible: false },
            { key: 'bon_space',          label: 'BonSpace',      defaultVisible: true },
            { key: 'bon_channel',        label: 'BonChannel',    defaultVisible: true },
            { key: 'priority',           label: '優先度',        defaultVisible: true },
            { key: 'failure_count',      label: '失敗回数',      defaultVisible: false },
            { key: 'scan_time',          label: 'スキャン日時',  defaultVisible: false },
            { key: 'last_seen',          label: '最終確認',      defaultVisible: false },
            { key: 'created_at',         label: '登録日時',      defaultVisible: false },
            { key: 'updated_at',         label: '更新日時',      defaultVisible: false },
            { key: 'actions',            label: '操作',          defaultVisible: true, locked: true },
        ];
        const CHANNEL_TABLE_COL_COUNT = CHANNEL_TABLE_COLUMNS.length;

        let channelsColumnVisibility = {};

        function loadChannelColumnPrefs() {
            try {
                const raw = localStorage.getItem('channelsTableColumnVisibility');
                if (!raw) return {};
                const parsed = JSON.parse(raw);
                return parsed && typeof parsed === 'object' ? parsed : {};
            } catch (_) {
                return {};
            }
        }

        function saveChannelColumnPrefs() {
            localStorage.setItem('channelsTableColumnVisibility', JSON.stringify(channelsColumnVisibility));
        }

        function applyChannelColumnVisibility() {
            // 列の表示/非表示は表示モードのみ対象（編集モードは独自レイアウト）
            if (channelEditMode) return;
            const table = document.getElementById('channels-table');
            if (!table) return;

            const isMobile = window.matchMedia('(max-width: 768px)').matches;
            const rows = table.querySelectorAll('tr');

            CHANNEL_TABLE_COLUMNS.forEach((colDef, idx) => {
                const visible = colDef.locked ? true : channelsColumnVisibility[colDef.key] !== false;
                rows.forEach(row => {
                    const cell = row.children[idx];
                    if (!cell) return;
                    // 空状態行など colspan セルは対象外
                    if (cell.colSpan && cell.colSpan > 1) return;
                    if (!visible) {
                        cell.style.display = 'none';
                    } else if (isMobile) {
                        cell.style.display = '';
                    } else {
                        cell.style.display = 'table-cell';
                    }
                });
            });
        }

        function initChannelsColumnPicker() {
            const picker = document.getElementById('channels-column-picker');
            if (!picker) return;

            const defaults = {};
            CHANNEL_TABLE_COLUMNS.forEach(c => { defaults[c.key] = c.defaultVisible !== false; });
            channelsColumnVisibility = Object.assign({}, defaults, loadChannelColumnPrefs());

            picker.innerHTML = CHANNEL_TABLE_COLUMNS.map(c => {
                const checked = c.locked ? true : channelsColumnVisibility[c.key] !== false;
                return `
                    <label>
                        <input type="checkbox" data-colkey="${c.key}" ${checked ? 'checked' : ''} ${c.locked ? 'disabled' : ''}>
                        ${escapeHtml(c.label)}
                    </label>
                `;
            }).join('');

            picker.querySelectorAll('input[type="checkbox"]').forEach(chk => {
                chk.addEventListener('change', (e) => {
                    channelsColumnVisibility[e.target.dataset.colkey] = !!e.target.checked;
                    saveChannelColumnPrefs();
                    applyChannelColumnVisibility();
                });
            });

            applyChannelColumnVisibility();
        }

        function getServiceTypeLabel(t) {
            if (t === null || t === undefined) return '-';
            switch (t) {
                case 0x01: return 'TV';
                case 0x02: return '音声';
                case 0xA1: return '臨時';
                case 0xA5: return 'プロモ';
                case 0xC0: return 'データ';
                default: return '0x' + t.toString(16).toUpperCase();
            }
        }

        function getChannelSortValue(channel, key) {
            switch (key) {
                case 'nid':
                    return channel.nid ?? -1;
                case 'sid':
                    return channel.sid ?? -1;
                case 'tsid':
                    return channel.tsid ?? -1;
                case 'channel_name':
                    return (channel.channel_name || channel.raw_name || '').toLowerCase();
                case 'terrestrial_region':
                    return (channel.terrestrial_region || '').toLowerCase();
                case 'network_name':
                    return (channel.network_name || '').toLowerCase();
                case 'tuner_count':
                    return channel.tuner_count ?? 0;
                case 'bon_space':
                    return channel.bon_space ?? -1;
                case 'bon_channel':
                    return channel.bon_channel ?? -1;
                case 'id':
                    return channel.id ?? -1;
                case 'raw_name':
                    return (channel.raw_name || '').toLowerCase();
                case 'manual_sheet':
                    return channel.manual_sheet ?? -1;
                case 'physical_ch':
                    return channel.physical_ch ?? -1;
                case 'remote_control_key':
                    return channel.remote_control_key ?? -1;
                case 'service_type':
                    return channel.service_type ?? -1;
                case 'bon_driver_path':
                    return (channel.bon_driver_path || (channel.tuner_names || []).join(', ') || '').toLowerCase();
                case 'failure_count':
                    return channel.failure_count ?? 0;
                case 'scan_time':
                    return channel.scan_time ?? 0;
                case 'last_seen':
                    return channel.last_seen ?? 0;
                case 'created_at':
                    return channel.created_at ?? 0;
                case 'updated_at':
                    return channel.updated_at ?? 0;
                default:
                    return channel[key];
            }
        }

        function normalizeChannelSortRules(rules) {
            const allowed = new Set([
                'is_enabled', 'channel_name', 'nid', 'sid', 'tsid', 'band_type',
                'terrestrial_region', 'network_name', 'tuner_count',
                'bon_space', 'bon_channel', 'priority',
                'id', 'raw_name', 'manual_sheet', 'physical_ch', 'remote_control_key',
                'service_type', 'bon_driver_path', 'failure_count',
                'scan_time', 'last_seen', 'created_at', 'updated_at'
            ]);

            const unique = [];
            const used = new Set();
            for (const rule of rules) {
                const key = rule?.key;
                if (!key || !allowed.has(key) || used.has(key)) continue;
                unique.push({ key, asc: rule.asc !== false });
                used.add(key);
                if (unique.length >= 3) break;
            }

            if (unique.length === 0) unique.push({ key: 'nid', asc: true });
            return unique;
        }

        function compareChannelValues(a, b) {
            let va = a;
            let vb = b;

            if (va === null || va === undefined) va = '';
            if (vb === null || vb === undefined) vb = '';

            if (typeof va === 'number' && typeof vb === 'number') {
                return va - vb;
            }
            if (typeof va === 'boolean' && typeof vb === 'boolean') {
                return va === vb ? 0 : (va ? -1 : 1);
            }

            const strA = String(va).toLowerCase();
            const strB = String(vb).toLowerCase();
            return strA.localeCompare(strB, 'ja');
        }

        // 表示モード用ヘッダー（静的HTMLのtheadと同一構成）
        function channelViewHeaderHtml() {
            return '<tr>' + CHANNEL_TABLE_COLUMNS.map(c =>
                c.key === 'actions'
                    ? `<th>${escapeHtml(c.label)}</th>`
                    : `<th class="sortable" data-sort="${c.key}">${escapeHtml(c.label)}</th>`
            ).join('') + '</tr>';
        }

        // 編集モード用ヘッダー（従来の11列レイアウト固定）
        const CHANNEL_EDIT_HEADER_HTML =
            '<tr>' +
            '<th class="sortable" data-sort="is_enabled">有効</th>' +
            '<th class="sortable" data-sort="channel_name">チャンネル名</th>' +
            '<th class="sortable" data-sort="nid">NID/SID/TSID</th>' +
            '<th class="sortable" data-sort="band_type">バンド</th>' +
            '<th class="sortable" data-sort="terrestrial_region">地域</th>' +
            '<th class="sortable" data-sort="network_name">ネットワーク</th>' +
            '<th class="sortable" data-sort="tuner_count">チューナー</th>' +
            '<th class="sortable" data-sort="bon_space">BonSpace</th>' +
            '<th class="sortable" data-sort="bon_channel">BonChannel</th>' +
            '<th class="sortable" data-sort="priority">優先度</th>' +
            '<th>操作</th>' +
            '</tr>';

        function renderChannels() {
            const tbody = document.getElementById('channels-body');
            const thead = document.querySelector('#channels-table thead');

            if (!channelEditMode) {
                // ---- 通常表示モード ----
                if (thead) thead.innerHTML = channelViewHeaderHtml();
                if (channelData.length === 0) {
                    tbody.innerHTML = `<tr><td colspan="${CHANNEL_TABLE_COL_COUNT}" class="empty-state">チャンネルがありません</td></tr>`;
                    applyResponsiveLabels('channels-table');
                    applyChannelColumnVisibility();
                    updateChannelSortIndicators();
                    return;
                }

                // Sort the data (multi-key)
                const rules = normalizeChannelSortRules(channelSortRules);
                const sorted = [...channelData].sort((a, b) => {
                    for (const rule of rules) {
                        const va = getChannelSortValue(a, rule.key);
                        const vb = getChannelSortValue(b, rule.key);
                        const cmp = compareChannelValues(va, vb);
                        if (cmp !== 0) return rule.asc ? cmp : -cmp;
                    }
                    return 0;
                });

                tbody.innerHTML = sorted.map(c => `
                    <tr ondblclick='enterChannelEditMode()'>
                        <td>${c.id}</td>
                        <td>
                            <label class="toggle">
                                <input type="checkbox" ${c.is_enabled ? 'checked' : ''} onchange="toggleChannel(${c.id}, this.checked)">
                                <span class="toggle-slider"></span>
                            </label>
                        </td>
                        <td>${getChannelLogoHtml(c)}${escapeHtml(c.channel_name || c.raw_name || '-')}</td>
                        <td>${escapeHtml(c.raw_name || '-')}</td>
                        <td><code>0x${c.nid.toString(16).toUpperCase().padStart(4,'0')}/${c.sid}/${c.tsid}</code></td>
                        <td>${c.manual_sheet !== null && c.manual_sheet !== undefined ? c.manual_sheet : '-'}</td>
                        <td><span class="badge ${getBandBadgeClass(c.band_type)}">${getBandTypeName(c.band_type)}</span></td>
                        <td>${escapeHtml(c.terrestrial_region || '-')}</td>
                        <td>${escapeHtml(c.network_name || '-')}</td>
                        <td>${c.physical_ch !== null && c.physical_ch !== undefined ? c.physical_ch : '-'}</td>
                        <td>${c.remote_control_key !== null && c.remote_control_key !== undefined ? c.remote_control_key : '-'}</td>
                        <td>${escapeHtml(getServiceTypeLabel(c.service_type))}</td>
                        <td>${c.tuner_count ? `<span class="badge badge-info" title="${escapeHtml((c.tuner_names || []).join(', '))}">${c.tuner_count}台</span>` : '-'}</td>
                        <td>${escapeHtml(c.bon_driver_path || (c.tuner_names || []).join(', ') || '-')}</td>
                        <td>${c.bon_space !== null && c.bon_space !== undefined ? c.bon_space : '-'}</td>
                        <td>${c.bon_channel !== null && c.bon_channel !== undefined ? c.bon_channel : '-'}</td>
                        <td>${c.priority}</td>
                        <td>${c.failure_count ?? 0}</td>
                        <td>${formatDateTime(c.scan_time)}</td>
                        <td>${formatDateTime(c.last_seen)}</td>
                        <td>${formatDateTime(c.created_at)}</td>
                        <td>${formatDateTime(c.updated_at)}</td>
                        <td>
                            <button class="btn btn-primary btn-sm" data-action="edit-channel" data-id="${c.id}">編集</button>
                            <button class="btn btn-secondary btn-sm" data-action="preview-channel" data-id="${c.id}">プレビュー</button>
                        </td>
                    </tr>
                `).join('');
                applyResponsiveLabels('channels-table');
                applyChannelColumnVisibility();
                updateChannelSortIndicators();
            } else {
                // ---- インライン編集モード ----
                // 編集モードは従来の11列固定レイアウト（列表示設定の対象外）
                if (thead) thead.innerHTML = CHANNEL_EDIT_HEADER_HTML;
                const rules = normalizeChannelSortRules(channelSortRules);
                const sorted = [...channelData].sort((a, b) => {
                    for (const rule of rules) {
                        const va = getChannelSortValue(a, rule.key);
                        const vb = getChannelSortValue(b, rule.key);
                        const cmp = compareChannelValues(va, vb);
                        if (cmp !== 0) return rule.asc ? cmp : -cmp;
                    }
                    return 0;
                });

                const existingRows = sorted.map(c => {
                    const edit = channelEdits[c.id] || {};
                    const isDeleted = edit.deleted === true;
                    const isModified = !isDeleted && Object.keys(edit).length > 0;
                    const dis = isDeleted ? 'disabled' : '';
                    const curName     = edit.channel_name  !== undefined ? edit.channel_name  : (c.channel_name || c.raw_name || '');
                    const curPriority = edit.priority       !== undefined ? edit.priority       : c.priority;
                    const curEnabled  = edit.is_enabled     !== undefined ? edit.is_enabled     : c.is_enabled;
                    const curNid      = edit.nid            !== undefined ? edit.nid            : c.nid;
                    const curSid      = edit.sid            !== undefined ? edit.sid            : c.sid;
                    const curTsid     = edit.tsid           !== undefined ? edit.tsid           : c.tsid;
                    const curBdId     = edit.bon_driver_id  !== undefined ? edit.bon_driver_id  : c.bon_driver_id;
                    const curSpace    = edit.bon_space      !== undefined ? edit.bon_space      : (c.bon_space  ?? '');
                    const curCh       = edit.bon_channel    !== undefined ? edit.bon_channel    : (c.bon_channel ?? '');
                    const rowClass = isDeleted ? 'ch-edit-row ch-deleted-row' : isModified ? 'ch-edit-row ch-modified-row' : 'ch-edit-row';

                    const bdOpts = bondriverList.map(bd =>
                        `<option value="${bd.id}" ${bd.id == curBdId ? 'selected' : ''}>${escapeHtml(bd.driver_name || bd.dll_path)}</option>`
                    ).join('');

                    return `
                        <tr class="${rowClass}" data-ch-id="${c.id}">
                            <td>
                                <label class="toggle">
                                    <input type="checkbox" ${curEnabled ? 'checked' : ''} onchange="onChEditField(${c.id},'is_enabled',this.checked)" ${dis}>
                                    <span class="toggle-slider"></span>
                                </label>
                            </td>
                            <td><input type="text" value="${escapeHtml(curName)}" placeholder="${escapeHtml(c.raw_name || '')}" oninput="onChEditField(${c.id},'channel_name',this.value)" ${dis}></td>
                            <td>
                                <div class="ch-new-ids">
                                    <label>NID</label><input type="number" min="0" max="65535" value="${curNid}" oninput="onChEditField(${c.id},'nid',+this.value)" ${dis}>
                                    <label>SID</label><input type="number" min="0" max="65535" value="${curSid}" oninput="onChEditField(${c.id},'sid',+this.value)" ${dis}>
                                    <label>TSID</label><input type="number" min="0" max="65535" value="${curTsid}" oninput="onChEditField(${c.id},'tsid',+this.value)" ${dis}>
                                </div>
                            </td>
                            <td><span class="badge ${getBandBadgeClass(c.band_type)}">${getBandTypeName(c.band_type)}</span></td>
                            <td>${escapeHtml(c.terrestrial_region || '-')}</td>
                            <td>${escapeHtml(c.network_name || '-')}</td>
                            <td>
                                ${bondriverList.length > 0
                                    ? `<select onchange="onChEditField(${c.id},'bon_driver_id',+this.value)" ${dis} style="font-size:11px;padding:3px 4px;max-width:130px;">${bdOpts}</select>`
                                    : (c.tuner_count ? `<span class="badge badge-info">${c.tuner_count}台</span>` : '-')
                                }
                            </td>
                            <td><input type="number" min="0" value="${curSpace}" placeholder="-" oninput="onChEditField(${c.id},'bon_space',this.value===''?null:+this.value)" ${dis} style="width:60px;padding:3px 6px;border:1px solid #ccc;border-radius:3px;font-size:12px;"></td>
                            <td><input type="number" min="0" value="${curCh}" placeholder="-" oninput="onChEditField(${c.id},'bon_channel',this.value===''?null:+this.value)" ${dis} style="width:60px;padding:3px 6px;border:1px solid #ccc;border-radius:3px;font-size:12px;"></td>
                            <td><input type="number" class="priority-input" value="${curPriority}" min="-100" max="100" oninput="onChEditField(${c.id},'priority',+this.value)" ${dis}></td>
                            <td>
                                ${isDeleted
                                    ? `<button class="btn btn-secondary btn-sm" onclick="onChUndoDelete(${c.id})">取消</button>`
                                    : `<button class="btn btn-danger btn-sm" onclick="onChMarkDelete(${c.id})">削除</button>`
                                }
                            </td>
                        </tr>
                    `;
                }).join('');

                const bdOptions = bondriverList.map(bd =>
                    `<option value="${bd.id}">${escapeHtml(bd.driver_name || bd.dll_path)}</option>`
                ).join('');

                const newRows = channelNewRows.map(row => `
                    <tr class="ch-edit-row ch-new-row" data-ch-temp="${row._tempId}">
                        <td>
                            <label class="toggle">
                                <input type="checkbox" checked onchange="onChNewEnabled(${row._tempId}, this.checked)">
                                <span class="toggle-slider"></span>
                            </label>
                        </td>
                        <td><input type="text" placeholder="チャンネル名" value="${escapeHtml(row.channel_name || '')}" oninput="onChNewField(${row._tempId}, 'channel_name', this.value)"></td>
                        <td>
                            <div class="ch-new-ids">
                                <label>NID</label><input type="number" min="0" max="65535" value="${row.nid || ''}" placeholder="NID" oninput="onChNewField(${row._tempId}, 'nid', this.value)">
                                <label>SID</label><input type="number" min="0" max="65535" value="${row.sid || ''}" placeholder="SID" oninput="onChNewField(${row._tempId}, 'sid', this.value)">
                                <label>TSID</label><input type="number" min="0" max="65535" value="${row.tsid || ''}" placeholder="TSID" oninput="onChNewField(${row._tempId}, 'tsid', this.value)">
                            </div>
                        </td>
                        <td>-</td>
                        <td>-</td>
                        <td>-</td>
                        <td>-</td>
                        <td><input type="number" min="0" value="${row.bon_space !== undefined ? row.bon_space : ''}" placeholder="Space" oninput="onChNewField(${row._tempId}, 'bon_space', this.value)" style="width:60px;padding:3px 6px;border:1px solid #ccc;border-radius:3px;font-size:12px;"></td>
                        <td><input type="number" min="0" value="${row.bon_channel !== undefined ? row.bon_channel : ''}" placeholder="Ch" oninput="onChNewField(${row._tempId}, 'bon_channel', this.value)" style="width:60px;padding:3px 6px;border:1px solid #ccc;border-radius:3px;font-size:12px;"></td>
                        <td><input type="number" class="priority-input" value="${row.priority || 0}" min="-100" max="100" oninput="onChNewField(${row._tempId}, 'priority', this.value)"></td>
                        <td>
                            <select onchange="onChNewField(${row._tempId}, 'bon_driver_id', this.value)" style="font-size:11px;padding:3px 4px;max-width:120px;">${bdOptions}</select>
                            <button class="btn btn-danger btn-sm" style="margin-top:2px;" onclick="removeChannelNewRow(${row._tempId})">削除</button>
                        </td>
                    </tr>
                `).join('');

                tbody.innerHTML = existingRows + newRows;
                if (tbody.innerHTML.trim() === '') {
                    tbody.innerHTML = '<tr><td colspan="11" class="empty-state">チャンネルがありません。「行を追加」で新規追加できます。</td></tr>';
                }
                applyResponsiveLabels('channels-table');
                updateChannelSortIndicators();
            }
        }

        function sortChannels(key) {
            channelSortRules = normalizeChannelSortRules(channelSortRules);
            const idx = channelSortRules.findIndex(r => r.key === key);
            if (idx === 0) {
                channelSortRules[0].asc = !channelSortRules[0].asc;
            } else {
                let asc = true;
                if (idx > 0) {
                    asc = channelSortRules[idx].asc;
                    channelSortRules.splice(idx, 1);
                }
                channelSortRules.unshift({ key, asc });
                channelSortRules = normalizeChannelSortRules(channelSortRules);
            }
            updateChannelSortIndicators();
            updateChannelSortUI();
            renderChannels();
        }

        function updateChannelSortIndicators() {
            document.querySelectorAll('#channels-table th.sortable').forEach(th => {
                th.classList.remove('asc', 'desc');
                th.removeAttribute('title');
            });

            const rules = normalizeChannelSortRules(channelSortRules);
            document.querySelectorAll('#channels-table th.sortable').forEach(th => {
                const key = th.dataset.sort;
                const idx = rules.findIndex(r => r.key === key);
                if (idx === 0) {
                    th.classList.add(rules[0].asc ? 'asc' : 'desc');
                    th.setAttribute('title', '第1ソートキー');
                } else if (idx > 0) {
                    th.setAttribute('title', `第${idx + 1}ソートキー`);
                }
            });
        }

        function updateChannelSortUI() {
            channelSortRules = normalizeChannelSortRules(channelSortRules);

            const key1 = document.getElementById('channel-sort-key-1');
            const key2 = document.getElementById('channel-sort-key-2');
            const key3 = document.getElementById('channel-sort-key-3');
            const order1 = document.getElementById('channel-sort-order-1');
            const order2 = document.getElementById('channel-sort-order-2');
            const order3 = document.getElementById('channel-sort-order-3');

            const r1 = channelSortRules[0];
            const r2 = channelSortRules[1];
            const r3 = channelSortRules[2];

            if (key1 && r1) key1.value = r1.key;
            if (key2) key2.value = r2 ? r2.key : '';
            if (key3) key3.value = r3 ? r3.key : '';

            if (order1 && r1) order1.textContent = `第1:${r1.asc ? '昇順' : '降順'}`;
            if (order2) {
                order2.disabled = !r2;
                order2.textContent = `第2:${r2 ? (r2.asc ? '昇順' : '降順') : '-'}`;
            }
            if (order3) {
                order3.disabled = !r3;
                order3.textContent = `第3:${r3 ? (r3.asc ? '昇順' : '降順') : '-'}`;
            }
        }

        function setChannelSortFromUI() {
            const key1 = document.getElementById('channel-sort-key-1')?.value;
            const key2 = document.getElementById('channel-sort-key-2')?.value;
            const key3 = document.getElementById('channel-sort-key-3')?.value;

            const oldAsc = new Map(normalizeChannelSortRules(channelSortRules).map(r => [r.key, r.asc]));
            channelSortRules = normalizeChannelSortRules([
                { key: key1, asc: oldAsc.has(key1) ? oldAsc.get(key1) : true },
                { key: key2, asc: oldAsc.has(key2) ? oldAsc.get(key2) : true },
                { key: key3, asc: oldAsc.has(key3) ? oldAsc.get(key3) : true },
            ]);

            updateChannelSortIndicators();
            updateChannelSortUI();
            renderChannels();
        }

        function toggleChannelSortOrder(index) {
            channelSortRules = normalizeChannelSortRules(channelSortRules);
            if (index < 0 || index >= channelSortRules.length) return;
            channelSortRules[index].asc = !channelSortRules[index].asc;
            updateChannelSortIndicators();
            updateChannelSortUI();
            renderChannels();
        }

        // Add click handlers to sortable headers.
        // thead はモード切替時に再生成されるため、テーブルへの委譲で処理する。
        (() => {
            const table = document.getElementById('channels-table');
            if (!table) return;
            table.addEventListener('click', (event) => {
                const th = event.target.closest('th.sortable');
                if (th && th.dataset.sort && table.contains(th)) {
                    sortChannels(th.dataset.sort);
                }
            });
        })();

        async function refreshChannels() {
            try {
                const bondriverId = document.getElementById('channel-bondriver-filter').value;
                const groupLogical = document.getElementById('channel-group-filter').checked;
                const enabledOnly = document.getElementById('channel-enabled-filter').checked;

                let url = '/api/channels?';
                if (bondriverId) url += `bondriver_id=${bondriverId}&`;
                if (!bondriverId || groupLogical) url += 'group_logical=true&';
                if (enabledOnly) url += 'enabled_only=true';

                const res = await fetch(url);
                const data = await res.json();

                if (!data.success || !data.channels) {
                    channelData = [];
                } else {
                    channelData = data.channels;
                }
                updateChannelSortIndicators();
                updateChannelSortUI();
                renderChannels();
            } catch (e) { console.error('Failed to refresh channels:', e); }
        }

        async function toggleChannel(id, enabled) {
            try {
                const res = await fetch(`/api/channel/${id}/toggle`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ enabled })
                });
                const data = await res.json();
                if (!data.success) alert('エラー: ' + data.error);
            } catch (e) { alert('更新に失敗しました: ' + e.message); }
        }

        // ============================================================
        // チャンネル インライン編集モード
        // ============================================================

        async function enterChannelEditMode() {
            if (channelEditMode) return;
            channelEditMode = true;
            channelEdits = {};
            channelNewRows = [];

            // BonDriverリストを取得（新規行のセレクタ用）
            try {
                const res = await fetch('/api/bondrivers');
                const data = await res.json();
                bondriverList = data.success ? (data.bondrivers || []) : [];
            } catch (_) { bondriverList = []; }

            document.getElementById('channel-view-controls').classList.add('hidden');
            document.getElementById('channel-edit-controls').classList.add('active');
            document.getElementById('ch-edit-save-msg').textContent = '';
            renderChannels();
        }

        function exitChannelEditMode() {
            channelEditMode = false;
            channelEdits = {};
            channelNewRows = [];
            document.getElementById('channel-edit-controls').classList.remove('active');
            document.getElementById('channel-view-controls').classList.remove('hidden');
            renderChannels();
        }

        function onChEditField(id, field, value) {
            if (!channelEdits[id]) channelEdits[id] = {};
            channelEdits[id][field] = value;
            markChRowModified(id);
        }

        function onChMarkDelete(id) {
            if (!channelEdits[id]) channelEdits[id] = {};
            channelEdits[id].deleted = true;
            const row = document.querySelector(`tr[data-ch-id="${id}"]`);
            if (row) {
                row.classList.remove('ch-modified-row');
                row.classList.add('ch-deleted-row');
                row.querySelectorAll('input').forEach(el => el.disabled = true);
                const btn = row.querySelector('td:last-child button');
                if (btn) { btn.className = 'btn btn-secondary btn-sm'; btn.textContent = '取消'; btn.onclick = () => onChUndoDelete(id); }
            }
        }

        function onChUndoDelete(id) {
            if (channelEdits[id]) delete channelEdits[id].deleted;
            if (channelEdits[id] && Object.keys(channelEdits[id]).length === 0) delete channelEdits[id];
            const row = document.querySelector(`tr[data-ch-id="${id}"]`);
            if (row) {
                row.classList.remove('ch-deleted-row');
                row.querySelectorAll('input').forEach(el => el.disabled = false);
                const edit = channelEdits[id];
                row.classList.toggle('ch-modified-row', edit && Object.keys(edit).length > 0);
                const btn = row.querySelector('td:last-child button');
                if (btn) { btn.className = 'btn btn-danger btn-sm'; btn.textContent = '削除'; btn.onclick = () => onChMarkDelete(id); }
            }
        }

        function markChRowModified(id) {
            const row = document.querySelector(`tr[data-ch-id="${id}"]`);
            if (row && !row.classList.contains('ch-deleted-row')) {
                row.classList.add('ch-modified-row');
            }
        }

        function addChannelRow() {
            const tempId = ++channelNewRowCounter;
            const defaultBdId = bondriverList.length > 0 ? bondriverList[0].id : null;
            channelNewRows.push({
                _tempId: tempId,
                bon_driver_id: defaultBdId,
                nid: '', sid: '', tsid: '',
                channel_name: '',
                bon_space: '', bon_channel: '',
                priority: 0,
                is_enabled: true,
            });
            renderChannels();
            // 最後の行の最初のinputにフォーカス
            const rows = document.querySelectorAll('tr[data-ch-temp]');
            if (rows.length > 0) {
                const lastRow = rows[rows.length - 1];
                const inp = lastRow.querySelector('input[type="text"]');
                if (inp) inp.focus();
            }
        }

        function removeChannelNewRow(tempId) {
            channelNewRows = channelNewRows.filter(r => r._tempId !== tempId);
            renderChannels();
        }

        function onChNewField(tempId, field, value) {
            const row = channelNewRows.find(r => r._tempId === tempId);
            if (!row) return;
            if (field === 'bon_driver_id' || field === 'nid' || field === 'sid' || field === 'tsid' || field === 'bon_space' || field === 'bon_channel' || field === 'priority') {
                row[field] = value === '' ? '' : (parseInt(value, 10) || 0);
            } else {
                row[field] = value;
            }
        }

        function onChNewEnabled(tempId, value) {
            const row = channelNewRows.find(r => r._tempId === tempId);
            if (row) row.is_enabled = value;
        }

        async function saveChannelEdits() {
            const msgEl = document.getElementById('ch-edit-save-msg');
            msgEl.textContent = '保存中...';
            msgEl.style.color = '#666';

            // 1. 既存チャンネルの一括更新
            const batchItems = Object.entries(channelEdits).map(([id, edit]) => ({
                id: parseInt(id, 10),
                channel_name: edit.channel_name,
                priority: edit.priority,
                is_enabled: edit.is_enabled,
                deleted: edit.deleted,
                bon_driver_id: edit.bon_driver_id,
                nid: edit.nid,
                sid: edit.sid,
                tsid: edit.tsid,
                bon_space: edit.bon_space,
                bon_channel: edit.bon_channel,
            }));

            let batchOk = true;
            if (batchItems.length > 0) {
                try {
                    const res = await fetch('/api/channels/batch', {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json' },
                        body: JSON.stringify(batchItems),
                    });
                    const data = await res.json();
                    if (!data.success) {
                        batchOk = false;
                        msgEl.textContent = 'エラー: ' + data.error;
                        msgEl.style.color = '#dc3545';
                        return;
                    }
                } catch (e) {
                    batchOk = false;
                    msgEl.textContent = '保存に失敗しました: ' + e.message;
                    msgEl.style.color = '#dc3545';
                    return;
                }
            }

            // 2. 新規チャンネルの作成
            let newErrors = [];
            for (const row of channelNewRows) {
                if (!row.bon_driver_id || row.nid === '' || row.sid === '' || row.tsid === '') {
                    newErrors.push('新規行: BonDriver・NID・SID・TSIDは必須です');
                    continue;
                }
                try {
                    const res = await fetch('/api/channel', {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json' },
                        body: JSON.stringify({
                            bon_driver_id: parseInt(row.bon_driver_id, 10),
                            nid: parseInt(row.nid, 10),
                            sid: parseInt(row.sid, 10),
                            tsid: parseInt(row.tsid, 10),
                            channel_name: row.channel_name || null,
                            bon_space: row.bon_space !== '' ? parseInt(row.bon_space, 10) : null,
                            bon_channel: row.bon_channel !== '' ? parseInt(row.bon_channel, 10) : null,
                            priority: parseInt(row.priority, 10) || 0,
                            is_enabled: row.is_enabled !== false,
                        }),
                    });
                    const data = await res.json();
                    if (!data.success) newErrors.push(data.error);
                } catch (e) {
                    newErrors.push(e.message);
                }
            }

            if (newErrors.length > 0) {
                msgEl.textContent = newErrors.join(' / ');
                msgEl.style.color = '#dc3545';
                return;
            }

            msgEl.textContent = '保存しました';
            msgEl.style.color = '#28a745';
            setTimeout(() => exitChannelEditMode(), 600);
            await refreshChannels();
        }

        // ============================================================
        // CSV エクスポート / インポート
        // ============================================================

        async function onChannelImport(input) {
            const file = input.files[0];
            if (!file) return;
            input.value = ''; // 同じファイルを再選択できるようリセット

            const text = await file.text();
            const resultEl = document.getElementById('channel-import-result');
            resultEl.innerHTML = '<p style="color:#666;">インポート中...</p>';
            openModal('channel-import-modal');

            try {
                const res = await fetch('/api/channels/import', {
                    method: 'POST',
                    headers: { 'Content-Type': 'text/csv; charset=utf-8' },
                    body: text,
                });
                const data = await res.json();

                let html = '';
                if (data.inserted !== undefined || data.updated !== undefined) {
                    html += `<p style="margin-bottom:8px;">`;
                    html += `<span style="color:#28a745;font-weight:600;">新規登録: ${data.inserted ?? 0} 件</span>　`;
                    html += `<span style="color:#667eea;font-weight:600;">更新: ${data.updated ?? 0} 件</span>`;
                    html += `</p>`;
                }
                if (data.errors && data.errors.length > 0) {
                    html += `<p style="color:#dc3545;font-weight:600;margin-bottom:4px;">エラー (${data.errors.length} 件):</p>`;
                    html += `<ul style="margin:0;padding-left:18px;font-size:12px;color:#dc3545;">`;
                    data.errors.forEach(e => { html += `<li>${escapeHtml(e)}</li>`; });
                    html += `</ul>`;
                } else if (!data.success) {
                    html += `<p style="color:#dc3545;">${escapeHtml(data.error || 'エラーが発生しました')}</p>`;
                }
                resultEl.innerHTML = html || '<p style="color:#28a745;">完了しました</p>';

                if ((data.inserted ?? 0) + (data.updated ?? 0) > 0) {
                    await refreshChannels();
                }
            } catch (e) {
                resultEl.innerHTML = `<p style="color:#dc3545;">通信エラー: ${escapeHtml(e.message)}</p>`;
            }
        }

        function editChannel(c) {
            document.getElementById('ch-id').value = c.id;
            document.getElementById('ch-info').value = `NID:${c.nid} SID:${c.sid} TSID:${c.tsid}`;
            document.getElementById('ch-name').value = c.channel_name || '';
            document.getElementById('ch-priority').value = c.priority;
            document.getElementById('ch-enabled').checked = c.is_enabled;
            openModal('channel-modal');
        }

        // ---- Browser preview (STREAMING_DESIGN.md §6.3/§6.4) ----
        // mpegts.js loads a converted (H.264) TS via `?profile=preview`,
        // sharing the same encoder pool as any BNDP session watching the
        // same channel (STREAMING_DESIGN.md §5 P4). Requires the
        // Authorization header, so the fetch/xhr loader is configured with
        // the same token `window.fetch` already injects for /api/* calls.
        let _previewPlayer = null;

        function openChannelPreview(sid, name) {
            document.getElementById('preview-title').textContent = 'プレビュー: ' + name;
            const statusEl = document.getElementById('preview-status');
            statusEl.textContent = '';
            openModal('channel-preview-modal');

            if (typeof mpegts === 'undefined' || !mpegts.isSupported()) {
                statusEl.textContent = 'mpegts.js が読み込めていないか、このブラウザでは再生できません。' +
                    'recisdb-proxy/static/mpegts.js を配置するか、ネットワーク接続（CDN）を確認してください。';
                return;
            }

            closeChannelPreview(/* keepModalOpen */ true);

            const token = getStoredAuthToken();
            const url = `/api/stream/service/${sid}?profile=preview`;
            try {
                _previewPlayer = mpegts.createPlayer(
                    { type: 'mpegts', isLive: true, url: url },
                    {
                        enableWorker: false,
                        liveBufferLatencyChasing: true,
                        // mpegts.js's fetch/xhr stream loader forwards this
                        // to every request it makes for `url` above — this
                        // is how the bearer token reaches an endpoint that
                        // sits behind the same auth as every other /api/*
                        // route (STREAMING_DESIGN.md §6.5). NOTE: not
                        // verified against a real mpegts.js build in this
                        // environment — if a future mpegts.js version drops
                        // `headers` support, fall back to `xhrSetup`.
                        headers: token ? { 'Authorization': 'Bearer ' + token } : {},
                    }
                );
                const video = document.getElementById('preview-video');
                _previewPlayer.attachMediaElement(video);
                _previewPlayer.load();
                _previewPlayer.play().catch(err => {
                    statusEl.textContent = '再生開始に失敗しました: ' + err.message;
                });
                _previewPlayer.on(mpegts.Events.ERROR, (type, detail) => {
                    statusEl.textContent = `再生エラー (${type}): ${JSON.stringify(detail)}`;
                });
            } catch (e) {
                statusEl.textContent = 'プレイヤーの初期化に失敗しました: ' + e.message;
            }
        }

        function closeChannelPreview(keepModalOpen) {
            if (_previewPlayer) {
                try { _previewPlayer.pause(); } catch (e) {}
                try { _previewPlayer.unload(); } catch (e) {}
                try { _previewPlayer.detachMediaElement(); } catch (e) {}
                try { _previewPlayer.destroy(); } catch (e) {}
                _previewPlayer = null;
            }
            const video = document.getElementById('preview-video');
            if (video) { video.removeAttribute('src'); video.load(); }
            if (!keepModalOpen) closeModal('channel-preview-modal');
        }

        document.getElementById('channel-form').onsubmit = async (e) => {
            e.preventDefault();
            const id = document.getElementById('ch-id').value;
            try {
                const res = await fetch(`/api/channel/${id}`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({
                        channel_name: document.getElementById('ch-name').value || null,
                        priority: parseInt(document.getElementById('ch-priority').value),
                        is_enabled: document.getElementById('ch-enabled').checked
                    })
                });
                const data = await res.json();
                if (data.success) {
                    closeModal('channel-modal');
                    refreshChannels();
                } else {
                    alert('エラー: ' + data.error);
                }
            } catch (e) { alert('保存に失敗しました: ' + e.message); }
        };

        async function deleteChannel() {
            if (!confirm('このチャンネルを削除しますか？')) return;
            const id = document.getElementById('ch-id').value;
            try {
                const res = await fetch(`/api/channel/${id}`, { method: 'DELETE' });
                const data = await res.json();
                if (data.success) {
                    closeModal('channel-modal');
                    refreshChannels();
                } else {
                    alert('エラー: ' + data.error);
                }
            } catch (e) { alert('削除に失敗しました: ' + e.message); }
        }

        // Scan History
        async function refreshHistory() {
            try {
                const res = await fetch('/api/scan-history');
                const data = await res.json();
                const tbody = document.getElementById('history-body');

                if (!data.success || !data.history || data.history.length === 0) {
                    tbody.innerHTML = '<tr><td colspan="5" class="empty-state">スキャン履歴がありません</td></tr>';
                    applyResponsiveLabels('history-table');
                    return;
                }

                tbody.innerHTML = data.history.map(h => `
                    <tr>
                        <td data-sort-value="${h.scan_time || 0}">${formatDateTime(h.scan_time)}</td>
                        <td data-sort-value="${h.bon_driver_id}">${h.bon_driver_id}</td>
                        <td data-sort-value="${h.success ? '1' : '0'}"><span class="badge ${h.success ? 'badge-success' : 'badge-danger'}">${h.success ? '成功' : '失敗'}</span></td>
                        <td data-sort-value="${h.channel_count !== null ? h.channel_count : -1}">${h.channel_count !== null ? h.channel_count : '-'}</td>
                        <td data-sort-value="${escapeHtml(h.error_message || '-')}">${escapeHtml(h.error_message) || '-'}</td>
                    </tr>
                `).join('');
                applyResponsiveLabels('history-table');
                sortTableRows('history-table');
            } catch (e) { console.error('Failed to refresh history:', e); }
        }

        // Session History
        async function refreshSessionHistory() {
            try {
                const address = document.getElementById('session-filter-address').value || '';
                const url = address ? `/api/session-history?client_address=${encodeURIComponent(address)}` : '/api/session-history';
                const res = await fetch(url);
                const data = await res.json();
                const tbody = document.getElementById('session-history-body');

                if (!data.success || !data.history || data.history.length === 0) {
                    tbody.innerHTML = '<tr><td colspan="10" class="empty-state">セッション履歴がありません</td></tr>';
                    applyResponsiveLabels('session-history-table');
                    return;
                }

                tbody.innerHTML = data.history.map(h => `
                    <tr>
                        <td data-sort-value="${h.started_at || 0}">${formatDateTime(h.started_at)}</td>
                        <td data-sort-value="${h.ended_at || 0}">${formatDateTime(h.ended_at)}</td>
                        <td data-sort-value="${escapeHtml(h.client_address)}">${escapeHtml(h.client_address)}</td>
                        <td data-sort-value="${escapeHtml(h.channel_name || h.channel_info || '-')}">${escapeHtml(h.channel_name || h.channel_info || '-') }</td>
                        <td data-sort-value="${h.duration_secs || 0}">${formatDuration(h.duration_secs)}</td>
                        <td data-sort-value="${h.packets_sent || 0}">${formatPackets(h.packets_sent)}</td>
                        <td data-sort-value="${h.packets_dropped || 0}">${formatPackets(h.packets_dropped)}</td>
                        <td data-sort-value="${h.packets_scrambled || 0}">${formatPackets(h.packets_scrambled)}</td>
                        <td data-sort-value="${h.packets_error || 0}">${formatPackets(h.packets_error)}</td>
                        <td data-sort-value="${h.average_bitrate_mbps !== null && h.average_bitrate_mbps !== undefined ? h.average_bitrate_mbps : 0}">${h.average_bitrate_mbps !== null && h.average_bitrate_mbps !== undefined ? h.average_bitrate_mbps.toFixed(2) + ' Mbps' : '-'}</td>
                    </tr>
                `).join('');
                applyResponsiveLabels('session-history-table');
                sortTableRows('session-history-table');
            } catch (e) { console.error('Failed to refresh session history:', e); }
        }

        // Alerts
        async function refreshAlerts() {
            try {
                const res = await fetch('/api/alerts');
                const data = await res.json();
                const tbody = document.getElementById('alerts-body');

                if (!data.success || !data.alerts || data.alerts.length === 0) {
                    tbody.innerHTML = '<tr><td colspan="5" class="empty-state">アクティブアラートはありません</td></tr>';
                    applyResponsiveLabels('alerts-table');
                    return;
                }

                tbody.innerHTML = data.alerts.map(a => `
                    <tr>
                        <td data-sort-value="${a.triggered_at || 0}">${formatDateTime(a.triggered_at)}</td>
                        <td data-sort-value="${a.rule_id}">${a.rule_id}</td>
                        <td data-sort-value="${a.session_id || 0}">${a.session_id || '-'}</td>
                        <td data-sort-value="${escapeHtml(a.message || '-')}">${escapeHtml(a.message || '-') }</td>
                        <td><button class="btn btn-success btn-sm" onclick="acknowledgeAlert(${a.id})">確認</button></td>
                    </tr>
                `).join('');
                applyResponsiveLabels('alerts-table');
                sortTableRows('alerts-table');
            } catch (e) { console.error('Failed to refresh alerts:', e); }
        }

        function formatMetricLabel(metric) {
            switch (metric) {
                case 'drop_rate': return 'Drop率';
                case 'scramble_rate': return 'Scramble率';
                case 'error_rate': return 'Error率';
                case 'signal_level': return '信号レベル';
                case 'bitrate': return 'ビットレート';
                default: return metric;
            }
        }

        function formatConditionLabel(condition) {
            switch (condition) {
                case 'gt': return 'より大きい (>)';
                case 'gte': return '以上 (>=)';
                case 'lt': return 'より小さい (<)';
                case 'lte': return '以下 (<=)';
                default: return condition;
            }
        }

        async function refreshAlertRules() {
            try {
                const res = await fetch('/api/alert-rules');
                const data = await res.json();
                const tbody = document.getElementById('alert-rules-body');

                if (!data.success || !data.rules || data.rules.length === 0) {
                    tbody.innerHTML = '<tr><td colspan="7" class="empty-state">ルールがありません</td></tr>';
                    applyResponsiveLabels('alert-rules-table');
                    return;
                }

                tbody.innerHTML = data.rules.map(r => `
                    <tr>
                        <td data-sort-value="${r.id}">${r.id}</td>
                        <td data-sort-value="${escapeHtml(r.name)}">${escapeHtml(r.name)}</td>
                        <td data-sort-value="${escapeHtml(r.metric)}">${escapeHtml(formatMetricLabel(r.metric))}</td>
                        <td data-sort-value="${escapeHtml(r.condition)}">${escapeHtml(formatConditionLabel(r.condition))}</td>
                        <td data-sort-value="${r.threshold}">${r.threshold}</td>
                        <td data-sort-value="${r.is_enabled ? '1' : '0'}"><span class="badge ${r.is_enabled ? 'badge-success' : 'badge-danger'}">${r.is_enabled ? 'ON' : 'OFF'}</span></td>
                        <td><button class="btn btn-danger btn-sm" onclick="deleteAlertRule(${r.id})">削除</button></td>
                    </tr>
                `).join('');
                applyResponsiveLabels('alert-rules-table');
                sortTableRows('alert-rules-table');
            } catch (e) { console.error('Failed to refresh alert rules:', e); }
        }

        async function acknowledgeAlert(id) {
            try {
                const res = await fetch(`/api/alerts/${id}/acknowledge`, { method: 'POST' });
                const data = await res.json();
                if (data.success) refreshAlerts();
            } catch (e) { alert('確認に失敗しました: ' + e.message); }
        }

        async function deleteAlertRule(id) {
            if (!confirm('このルールを削除しますか？')) return;
            try {
                const res = await fetch(`/api/alert-rules/${id}`, { method: 'DELETE' });
                const data = await res.json();
                if (data.success) refreshAlertRules();
            } catch (e) { alert('削除に失敗しました: ' + e.message); }
        }

        document.getElementById('alert-rule-form').onsubmit = async (e) => {
            e.preventDefault();
            try {
                const res = await fetch('/api/alert-rules', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({
                        name: document.getElementById('ar-name').value,
                        metric: document.getElementById('ar-metric').value,
                        condition: document.getElementById('ar-condition').value,
                        threshold: parseFloat(document.getElementById('ar-threshold').value),
                        severity: 'warning',
                        is_enabled: document.getElementById('ar-enabled').checked,
                        webhook_url: document.getElementById('ar-webhook-url').value || null,
                        webhook_format: document.getElementById('ar-webhook-format').value
                    })
                });
                const data = await res.json();
                if (data.success) {
                    closeModal('alert-rule-modal');
                    refreshAlertRules();
                } else {
                    alert('エラー: ' + data.error);
                }
            } catch (e) { alert('保存に失敗しました: ' + e.message); }
        };

        // Scan Config Functions
        async function loadScanConfig() {
            try {
                const response = await fetch('/api/scan-config');
                const data = await response.json();
                if (data.success && data.config) {
                    document.getElementById('check-interval').value = data.config.check_interval_secs;
                    document.getElementById('max-concurrent').value = data.config.max_concurrent_scans;
                    document.getElementById('scan-timeout').value = data.config.scan_timeout_secs;
                    document.getElementById('signal-lock-wait').value = data.config.signal_lock_wait_ms ?? 500;
                    document.getElementById('ts-read-timeout').value = data.config.ts_read_timeout_ms ?? 300000;
                    hideConfigMessage();
                }
            } catch (e) { console.error('Failed to load scan config:', e); }
        }

        async function saveScanConfig() {
            const config = {
                check_interval_secs: parseInt(document.getElementById('check-interval').value),
                max_concurrent_scans: parseInt(document.getElementById('max-concurrent').value),
                scan_timeout_secs: parseInt(document.getElementById('scan-timeout').value),
                signal_lock_wait_ms: parseInt(document.getElementById('signal-lock-wait').value),
                ts_read_timeout_ms: parseInt(document.getElementById('ts-read-timeout').value)
            };

            if (
                config.check_interval_secs <= 0 ||
                config.max_concurrent_scans <= 0 ||
                config.scan_timeout_secs <= 0 ||
                config.signal_lock_wait_ms <= 0 ||
                config.ts_read_timeout_ms <= 0
            ) {
                showConfigMessage('すべてのフィールドに正の数値を入力してください', 'error');
                return;
            }

            try {
                const response = await fetch('/api/scan-config', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(config)
                });
                const data = await response.json();
                if (data.success) {
                    showConfigMessage('設定を保存しました', 'success');
                } else {
                    showConfigMessage('設定の保存に失敗しました: ' + (data.error || 'Unknown error'), 'error');
                }
            } catch (e) {
                showConfigMessage('設定の保存に失敗しました: ' + e.message, 'error');
            }
        }

        function showConfigMessage(message, type) {
            const msgEl = document.getElementById('config-message');
            msgEl.textContent = message;
            msgEl.style.display = 'block';
            msgEl.style.padding = '10px 12px';
            msgEl.style.borderRadius = '4px';
            msgEl.style.fontSize = '13px';
            if (type === 'success') {
                msgEl.style.background = '#d4edda';
                msgEl.style.color = '#155724';
            } else {
                msgEl.style.background = '#f8d7da';
                msgEl.style.color = '#721c24';
            }
            setTimeout(hideConfigMessage, 5000);
        }

        function hideConfigMessage() {
            document.getElementById('config-message').style.display = 'none';
        }

        // Tuner Config Functions
        async function loadTunerConfig() {
            try {
                const response = await fetch('/api/tuner-config');
                const data = await response.json();
                if (data.success && data.config) {
                    document.getElementById('tuner-keep-alive').value = data.config.keep_alive_secs;
                    document.getElementById('tuner-prewarm-enabled').checked = !!data.config.prewarm_enabled;
                    document.getElementById('tuner-prewarm-timeout').value = data.config.prewarm_timeout_secs;
                    document.getElementById('tuner-setch-retry-interval').value = data.config.set_channel_retry_interval_ms ?? 500;
                    document.getElementById('tuner-setch-retry-timeout').value = data.config.set_channel_retry_timeout_ms ?? 10000;
                    document.getElementById('tuner-signal-poll-interval').value = data.config.signal_poll_interval_ms ?? 500;
                    document.getElementById('tuner-signal-wait-timeout').value = data.config.signal_wait_timeout_ms ?? 10000;
                    document.getElementById('tuner-prefill-view-ms').value = data.config.prefill_view_ms ?? 1000;
                    document.getElementById('tuner-prefill-preview-ms').value = data.config.prefill_preview_ms ?? 2000;
                    document.getElementById('tuner-prefill-record-ms').value = data.config.prefill_record_ms ?? 6000;
                    document.getElementById('tuner-jitter-safety-factor').value = data.config.jitter_safety_factor ?? 1.5;
                    hideTunerConfigMessage();
                }
            } catch (e) { console.error('Failed to load tuner config:', e); }
        }

        async function saveTunerConfig() {
            const config = {
                keep_alive_secs: parseInt(document.getElementById('tuner-keep-alive').value),
                prewarm_enabled: document.getElementById('tuner-prewarm-enabled').checked,
                prewarm_timeout_secs: parseInt(document.getElementById('tuner-prewarm-timeout').value),
                set_channel_retry_interval_ms: parseInt(document.getElementById('tuner-setch-retry-interval').value),
                set_channel_retry_timeout_ms: parseInt(document.getElementById('tuner-setch-retry-timeout').value),
                signal_poll_interval_ms: parseInt(document.getElementById('tuner-signal-poll-interval').value),
                signal_wait_timeout_ms: parseInt(document.getElementById('tuner-signal-wait-timeout').value),
                prefill_view_ms: parseInt(document.getElementById('tuner-prefill-view-ms').value),
                prefill_preview_ms: parseInt(document.getElementById('tuner-prefill-preview-ms').value),
                prefill_record_ms: parseInt(document.getElementById('tuner-prefill-record-ms').value),
                jitter_safety_factor: parseFloat(document.getElementById('tuner-jitter-safety-factor').value)
            };

            if (
                config.keep_alive_secs < 0 ||
                config.prewarm_timeout_secs <= 0 ||
                config.set_channel_retry_interval_ms <= 0 ||
                config.set_channel_retry_timeout_ms <= 0 ||
                config.signal_poll_interval_ms <= 0 ||
                config.signal_wait_timeout_ms <= 0 ||
                config.prefill_view_ms < 0 ||
                config.prefill_preview_ms < 0 ||
                config.prefill_record_ms < 0 ||
                config.jitter_safety_factor <= 0
            ) {
                showTunerConfigMessage('入力値を確認してください', 'error');
                return;
            }

            try {
                const response = await fetch('/api/tuner-config', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(config)
                });
                const data = await response.json();
                if (data.success) {
                    showTunerConfigMessage('設定を保存しました', 'success');
                } else {
                    showTunerConfigMessage('設定の保存に失敗しました: ' + (data.error || 'Unknown error'), 'error');
                }
            } catch (e) {
                showTunerConfigMessage('設定の保存に失敗しました: ' + e.message, 'error');
            }
        }

        function showTunerConfigMessage(message, type) {
            const msgEl = document.getElementById('tuner-config-message');
            msgEl.textContent = message;
            msgEl.style.display = 'block';
            msgEl.style.padding = '10px 12px';
            msgEl.style.borderRadius = '4px';
            msgEl.style.fontSize = '13px';
            if (type === 'success') {
                msgEl.style.background = '#d4edda';
                msgEl.style.color = '#155724';
            } else {
                msgEl.style.background = '#f8d7da';
                msgEl.style.color = '#721c24';
            }
            setTimeout(hideTunerConfigMessage, 5000);
        }

        function hideTunerConfigMessage() {
            document.getElementById('tuner-config-message').style.display = 'none';
        }

        // tsreplace Config Functions
        async function loadTsreplaceConfig() {
            try {
                const response = await fetch('/api/tsreplace-config');
                const data = await response.json();
                if (data.success && data.config) {
                    document.getElementById('tsreplace-enabled').checked = !!data.config.enabled;
                    document.getElementById('tsreplace-command-path').value = data.config.command_path || 'tsreplace';
                    document.getElementById('tsreplace-arguments').value = data.config.arguments || '';
                    document.getElementById('tsreplace-read-timeout').value = data.config.read_timeout_ms ?? 10000;
                    document.getElementById('tsreplace-max-encoders').value = data.config.max_concurrent_encoders ?? 2;
                    document.getElementById('tsreplace-passthrough-on-error').checked = !!data.config.passthrough_on_error;
                    document.getElementById('tsreplace-preprocessor-path').value = data.config.preprocessor_path || '';
                    document.getElementById('tsreplace-preprocessor-arguments').value = data.config.preprocessor_arguments || '';
                    hideTsreplaceConfigMessage();
                }
            } catch (e) {
                console.error('Failed to load tsreplace config:', e);
            }
        }

        async function saveTsreplaceConfig() {
            // Note: command_path is read-only in this UI and is not sent —
            // it can only be changed via recisdb-proxy.toml [tsreplace]
            // command_path (REVIEW_2026-07.md S1). The server ignores this
            // field even if present, but we avoid sending it at all so the
            // intent is unambiguous.
            const readTimeoutMs = parseInt(document.getElementById('tsreplace-read-timeout').value, 10);
            const maxEncoders = parseInt(document.getElementById('tsreplace-max-encoders').value, 10);

            if (!Number.isFinite(readTimeoutMs) || readTimeoutMs <= 0) {
                showTsreplaceConfigMessage('読み取りタイムアウトは正の数値を入力してください', 'error');
                return;
            }
            if (!Number.isFinite(maxEncoders) || maxEncoders <= 0) {
                showTsreplaceConfigMessage('同時エンコード数の上限は正の数値を入力してください', 'error');
                return;
            }

            const payload = {
                enabled: document.getElementById('tsreplace-enabled').checked,
                arguments: document.getElementById('tsreplace-arguments').value,
                read_timeout_ms: readTimeoutMs,
                passthrough_on_error: document.getElementById('tsreplace-passthrough-on-error').checked,
                max_concurrent_encoders: maxEncoders,
                // preprocessor_path is read-only here (TOML-only, like
                // command_path); only its arguments are editable.
                preprocessor_arguments: document.getElementById('tsreplace-preprocessor-arguments').value,
            };

            try {
                const response = await fetch('/api/tsreplace-config', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(payload)
                });
                const data = await response.json();
                if (data.success) {
                    showTsreplaceConfigMessage('設定を保存しました', 'success');
                } else {
                    showTsreplaceConfigMessage('設定の保存に失敗しました: ' + (data.error || 'Unknown error'), 'error');
                }
            } catch (e) {
                showTsreplaceConfigMessage('設定の保存に失敗しました: ' + e.message, 'error');
            }
        }

        function showTsreplaceConfigMessage(message, type) {
            const msgEl = document.getElementById('tsreplace-config-message');
            msgEl.textContent = message;
            msgEl.style.display = 'block';
            msgEl.style.padding = '10px 12px';
            msgEl.style.borderRadius = '4px';
            msgEl.style.fontSize = '13px';
            if (type === 'success') {
                msgEl.style.background = '#d4edda';
                msgEl.style.color = '#155724';
            } else {
                msgEl.style.background = '#f8d7da';
                msgEl.style.color = '#721c24';
            }
            setTimeout(hideTsreplaceConfigMessage, 5000);
        }

        function hideTsreplaceConfigMessage() {
            document.getElementById('tsreplace-config-message').style.display = 'none';
        }

        // Browser preview (?profile=preview) Config Functions — fully
        // separate from the BNDP tsreplace config above.
        async function loadPreviewConfig() {
            try {
                const response = await fetch('/api/preview-config');
                const data = await response.json();
                if (data.success && data.config) {
                    document.getElementById('preview-enabled').checked = !!data.config.enabled;
                    document.getElementById('preview-command-path').value = data.config.command_path || '';
                    document.getElementById('preview-preprocessor-path').value = data.config.preprocessor_path || '';
                    document.getElementById('preview-preprocessor-arguments').value = data.config.preprocessor_arguments || '';
                    document.getElementById('preview-read-timeout').value = data.config.read_timeout_ms ?? 10000;
                    hidePreviewConfigMessage();
                }
            } catch (e) {
                console.error('Failed to load preview config:', e);
            }
        }

        async function savePreviewConfig() {
            // command_path / preprocessor_path are read-only here (TOML-only,
            // recisdb-proxy.toml [preview], REVIEW S1) and are not sent.
            const readTimeoutMs = parseInt(document.getElementById('preview-read-timeout').value, 10);
            if (!Number.isFinite(readTimeoutMs) || readTimeoutMs <= 0) {
                showPreviewConfigMessage('読み取りタイムアウトは正の数値を入力してください', 'error');
                return;
            }

            const payload = {
                enabled: document.getElementById('preview-enabled').checked,
                preprocessor_arguments: document.getElementById('preview-preprocessor-arguments').value,
                read_timeout_ms: readTimeoutMs,
            };

            try {
                const response = await fetch('/api/preview-config', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(payload)
                });
                const data = await response.json();
                if (data.success) {
                    showPreviewConfigMessage('設定を保存しました', 'success');
                } else {
                    showPreviewConfigMessage('設定の保存に失敗しました: ' + (data.error || 'Unknown error'), 'error');
                }
            } catch (e) {
                showPreviewConfigMessage('設定の保存に失敗しました: ' + e.message, 'error');
            }
        }

        function showPreviewConfigMessage(message, type) {
            const msgEl = document.getElementById('preview-config-message');
            msgEl.textContent = message;
            msgEl.style.display = 'block';
            msgEl.style.padding = '10px 12px';
            msgEl.style.borderRadius = '4px';
            msgEl.style.fontSize = '13px';
            if (type === 'success') {
                msgEl.style.background = '#d4edda';
                msgEl.style.color = '#155724';
            } else {
                msgEl.style.background = '#f8d7da';
                msgEl.style.color = '#721c24';
            }
            setTimeout(hidePreviewConfigMessage, 5000);
        }

        function hidePreviewConfigMessage() {
            document.getElementById('preview-config-message').style.display = 'none';
        }

        // ---- Encode profiles (STREAMING_DESIGN.md §5.3/§9 P5) ----
        let encodeProfileData = [];

        async function refreshEncodeProfiles() {
            try {
                const res = await fetch('/api/encode-profiles');
                const data = await res.json();
                if (data.success) {
                    encodeProfileData = data.profiles || [];
                    renderEncodeProfiles();
                }
            } catch (e) {
                console.error('Failed to load encode profiles:', e);
            }
        }

        function renderEncodeProfiles() {
            const tbody = document.getElementById('encode-profiles-body');
            if (!tbody) return;
            if (encodeProfileData.length === 0) {
                tbody.innerHTML = '<tr><td colspan="8" class="empty-state">プロファイルがありません</td></tr>';
                return;
            }
            tbody.innerHTML = encodeProfileData.map(p => `
                <tr>
                    <td>
                        <label class="toggle">
                            <input type="checkbox" ${p.is_enabled ? 'checked' : ''} onchange="toggleEncodeProfile(${p.id}, this.checked)">
                            <span class="toggle-slider"></span>
                        </label>
                    </td>
                    <td>${escapeHtml(p.name)}</td>
                    <td>${escapeHtml(p.purpose)}</td>
                    <td>${escapeHtml(p.codec)}</td>
                    <td>${escapeHtml(p.container || 'mpegts')}</td>
                    <td>${p.target_bitrate ? Math.round(p.target_bitrate / 1000) + ' kbps' : '-'}</td>
                    <td style="max-width:260px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;" title="${escapeHtml(p.extra_args || '')}">${escapeHtml(p.extra_args || '-')}</td>
                    <td>
                        <button class="btn btn-primary btn-sm" data-action="edit-encode-profile" data-id="${p.id}">編集</button>
                    </td>
                </tr>
            `).join('');
        }

        function openCreateEncodeProfile() {
            document.getElementById('encode-profile-modal-title').textContent = 'エンコードプロファイル追加';
            document.getElementById('ep-id').value = '';
            document.getElementById('ep-name').value = '';
            document.getElementById('ep-purpose').value = 'preview';
            document.getElementById('ep-codec').value = 'h264';
            document.getElementById('ep-container').value = 'mpegts';
            document.getElementById('ep-bitrate').value = '';
            document.getElementById('ep-extra-args').value = '';
            document.getElementById('ep-enabled').checked = true;
            document.getElementById('ep-delete-btn').style.display = 'none';
            openModal('encode-profile-modal');
        }

        function openEditEncodeProfile(p) {
            document.getElementById('encode-profile-modal-title').textContent = 'エンコードプロファイル編集';
            document.getElementById('ep-id').value = p.id;
            document.getElementById('ep-name').value = p.name;
            document.getElementById('ep-purpose').value = p.purpose;
            document.getElementById('ep-codec').value = p.codec;
            document.getElementById('ep-container').value = p.container || 'mpegts';
            document.getElementById('ep-bitrate').value = p.target_bitrate ?? '';
            document.getElementById('ep-extra-args').value = p.extra_args || '';
            document.getElementById('ep-enabled').checked = !!p.is_enabled;
            document.getElementById('ep-delete-btn').style.display = '';
            openModal('encode-profile-modal');
        }

        async function toggleEncodeProfile(id, enabled) {
            try {
                const res = await fetch(`/api/encode-profiles/${id}`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ is_enabled: enabled })
                });
                const data = await res.json();
                if (!data.success) alert('エラー: ' + data.error);
                refreshEncodeProfiles();
            } catch (e) { alert('更新に失敗しました: ' + e.message); }
        }

        async function deleteEncodeProfile() {
            const id = document.getElementById('ep-id').value;
            if (!id) return;
            if (!confirm('このプロファイルを削除しますか？')) return;
            try {
                const res = await fetch(`/api/encode-profiles/${id}`, { method: 'DELETE' });
                const data = await res.json();
                if (data.success) {
                    closeModal('encode-profile-modal');
                    refreshEncodeProfiles();
                } else {
                    alert('エラー: ' + data.error);
                }
            } catch (e) { alert('削除に失敗しました: ' + e.message); }
        }

        document.getElementById('encode-profile-form').onsubmit = async (e) => {
            e.preventDefault();
            const id = document.getElementById('ep-id').value;
            const bitrateRaw = document.getElementById('ep-bitrate').value;
            const argsRaw = document.getElementById('ep-extra-args').value;
            const payload = {
                name: document.getElementById('ep-name').value,
                purpose: document.getElementById('ep-purpose').value,
                codec: document.getElementById('ep-codec').value,
                container: document.getElementById('ep-container').value || 'mpegts',
                target_bitrate: bitrateRaw === '' ? null : parseInt(bitrateRaw, 10),
                extra_args: argsRaw === '' ? null : argsRaw,
                is_enabled: document.getElementById('ep-enabled').checked,
            };
            try {
                const url = id ? `/api/encode-profiles/${id}` : '/api/encode-profiles';
                const res = await fetch(url, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(payload)
                });
                const data = await res.json();
                if (data.success) {
                    closeModal('encode-profile-modal');
                    refreshEncodeProfiles();
                } else {
                    alert('エラー: ' + data.error);
                }
            } catch (e) { alert('保存に失敗しました: ' + e.message); }
        };

        // ============================================================
        // クライアント設定ガイド
        // ============================================================
        let clientGuideProxyPort = null;
        let clientGuideTargets = [];
        let clientGuideSelected = null;

        function basename(path) {
            if (!path) return '';
            const parts = String(path).split(/[\\/]/);
            return parts[parts.length - 1] || path;
        }

        async function refreshClientGuide() {
            const tbody = document.getElementById('client-guide-targets-body');
            try {
                const res = await fetch('/api/client-view/targets');
                const data = await res.json();
                if (!data.success) {
                    tbody.innerHTML = `<tr><td colspan="5" class="empty-state">読み込みエラー: ${escapeHtml(data.error || '')}</td></tr>`;
                    return;
                }
                clientGuideProxyPort = data.proxy_port;
                clientGuideTargets = data.targets || [];

                if (clientGuideTargets.length === 0) {
                    tbody.innerHTML = '<tr><td colspan="5" class="empty-state">BonDriverが登録されていません。先に「BonDriver」タブでチューナーを登録・スキャンしてください</td></tr>';
                    renderClientGuideIni();
                    return;
                }

                // 前回の選択を維持。なければ有効チャンネルを持つ最初のターゲット
                // (グループが先頭に並ぶのでグループ優先) を自動選択。
                if (!clientGuideSelected || !clientGuideTargets.some(t => t.name === clientGuideSelected)) {
                    const firstUsable = clientGuideTargets.find(t => t.enabled_channels > 0) || clientGuideTargets[0];
                    clientGuideSelected = firstUsable.name;
                }

                renderClientGuideTargets();
                renderClientGuideIni();
                await loadClientGuideView();
            } catch (e) {
                tbody.innerHTML = `<tr><td colspan="5" class="empty-state">読み込みエラー: ${escapeHtml(e.message)}</td></tr>`;
            }
        }

        function renderClientGuideTargets() {
            const tbody = document.getElementById('client-guide-targets-body');
            // Selection is index-based: tuner names are Windows DLL paths
            // whose backslashes/quotes must never be embedded in inline JS.
            tbody.innerHTML = clientGuideTargets.map((t, i) => {
                const checked = t.name === clientGuideSelected ? 'checked' : '';
                const kind = t.type === 'group'
                    ? '<span class="badge badge-success">グループ（推奨）</span>'
                    : 'チューナー単体';
                let note = '';
                if (t.type === 'group') {
                    note = `${(t.drivers || []).map(basename).map(escapeHtml).join(', ')}`;
                } else if (t.display_name) {
                    note = `表示名: ${escapeHtml(t.display_name)}`;
                }
                if (!t.enabled_channels) {
                    note += (note ? ' / ' : '') + '<span style="color:#e67e22;">有効チャンネルなし（スキャン未実施?）</span>';
                }
                return `
                    <tr style="cursor:pointer;" onclick="selectClientGuideTarget(${i})">
                        <td><input type="radio" name="client-guide-target" ${checked}></td>
                        <td><code style="user-select: all;">${escapeHtml(t.name)}</code></td>
                        <td>${kind}</td>
                        <td>${t.enabled_channels}</td>
                        <td>${note || '-'}</td>
                    </tr>`;
            }).join('');
            applyResponsiveLabels('client-guide-targets-table');
        }

        async function selectClientGuideTarget(index) {
            const target = clientGuideTargets[index];
            if (!target) return;
            clientGuideSelected = target.name;
            renderClientGuideTargets();
            renderClientGuideIni();
            await loadClientGuideView();
        }

        function clientGuideIniText() {
            const host = location.hostname || '127.0.0.1';
            const port = clientGuideProxyPort || 40070;
            const tuner = clientGuideSelected || '(STEP 1 でチューナーを選択)';
            return `[Server]\n` +
                `Address = ${host}:${port}\n` +
                `Tuner = ${tuner}\n`;
        }

        function renderClientGuideIni() {
            document.getElementById('client-guide-ini').textContent = clientGuideIniText();
        }

        async function copyClientGuideIni(btn) {
            try {
                await navigator.clipboard.writeText(clientGuideIniText());
                const orig = btn.textContent;
                btn.textContent = 'コピーしました！';
                setTimeout(() => { btn.textContent = orig; }, 1500);
            } catch (e) {
                alert('コピーに失敗しました。表示内容を手動で選択してコピーしてください。');
            }
        }

        // 認証ヘッダを通すため <a href> ではなく fetch + blob でダウンロードする
        async function downloadClientFile(kind) {
            const msg = document.getElementById('client-guide-download-msg');
            if (!clientGuideSelected) {
                msg.textContent = '先に STEP 1 でチューナーを選択してください';
                return;
            }
            msg.textContent = '生成中...';
            try {
                const res = await fetch(`/api/client-view/files/${kind}?tuner=${encodeURIComponent(clientGuideSelected)}`);
                if (!res.ok) {
                    let detail = '';
                    try { detail = (await res.json()).error || ''; } catch (e2) {}
                    msg.textContent = `ダウンロードに失敗しました (HTTP ${res.status}) ${detail}`;
                    return;
                }
                const fallbackNames = {
                    'tvtest-ch2': 'BonDriver_NetworkProxy.ch2',
                    'chset4': 'BonDriver_NetworkProxy(BonDriver_NetworkProxy).ChSet4.txt',
                    'chset5': 'ChSet5.txt',
                    'bundle': 'recisdb-proxy-client-config.zip',
                };
                const disposition = res.headers.get('Content-Disposition') || '';
                const m = disposition.match(/filename\*=UTF-8''([^;]+)/i) || disposition.match(/filename="?([^";]+)"?/i);
                const filename = m ? decodeURIComponent(m[1]) : fallbackNames[kind];
                const blob = await res.blob();
                const url = URL.createObjectURL(blob);
                const a = document.createElement('a');
                a.href = url;
                a.download = filename;
                document.body.appendChild(a);
                a.click();
                a.remove();
                URL.revokeObjectURL(url);
                msg.textContent = `${filename} を保存しました`;
            } catch (e) {
                msg.textContent = 'ダウンロードに失敗しました: ' + e.message;
            }
        }

        async function loadClientGuideView() {
            const view = document.getElementById('client-guide-view');
            if (!clientGuideSelected) return;
            view.innerHTML = '<div class="loading">読み込み中...</div>';
            try {
                const res = await fetch(`/api/client-view?tuner=${encodeURIComponent(clientGuideSelected)}`);
                const data = await res.json();
                if (!data.success) {
                    view.innerHTML = `<div class="empty-state">${escapeHtml(data.error || '読み込みエラー')}</div>`;
                    return;
                }
                if (!data.spaces || data.spaces.length === 0) {
                    view.innerHTML = '<div class="empty-state">このチューナーには有効なチャンネルがありません。「BonDriver」タブでスキャンを実行するか、「チャンネル」タブでチャンネルを有効にしてください</div>';
                    return;
                }
                view.innerHTML = data.spaces.map(space => {
                    const rows = space.channels.map(ch => {
                        const phys = (ch.physical || [])
                            .map(p => `${escapeHtml(basename(p.driver))} (Space ${p.space} / Ch ${p.channel})`)
                            .join('<br>');
                        return `
                            <tr>
                                <td>${ch.index}</td>
                                <td>${escapeHtml(ch.name)}</td>
                                <td>${phys || '-'}</td>
                            </tr>`;
                    }).join('');
                    return `
                        <h5 style="margin: 18px 0 6px;">チューニング空間 ${space.index}: ${escapeHtml(space.name)}</h5>
                        <table class="responsive-table">
                            <thead>
                                <tr>
                                    <th style="width: 90px;">CH番号</th>
                                    <th>チャンネル名（クライアントに表示される名前）</th>
                                    <th>物理チューナー（参考）</th>
                                </tr>
                            </thead>
                            <tbody>${rows}</tbody>
                        </table>`;
                }).join('');
            } catch (e) {
                view.innerHTML = `<div class="empty-state">読み込みエラー: ${escapeHtml(e.message)}</div>`;
            }
        }

        // Initialize
        window.addEventListener('load', () => {
            updateThemeButton();
            initClientsColumnPicker();
            initChannelsColumnPicker();
            refreshStats();
            refreshClients();
            loadScanConfig();
            loadTunerConfig();
            loadTsreplaceConfig();
            loadPreviewConfig();
            refreshEncodeProfiles();
            enableTableSorting('clients-table');
            enableTableSorting('bondrivers-table');
            enableTableSorting('history-table');
            enableTableSorting('session-history-table');
            enableTableSorting('alerts-table');
            enableTableSorting('alert-rules-table');
            setInterval(() => { refreshStats(); refreshClients(); updateClientMetrics(); }, 2000);
        });

        // テーマ切替
        function toggleTheme() {
            const html = document.documentElement;
            const isModern = html.classList.toggle('theme-modern');
            localStorage.setItem('dashboardTheme', isModern ? 'modern' : 'classic');
            updateThemeButton();
        }
        function updateThemeButton() {
            const btn = document.getElementById('theme-toggle-btn');
            if (!btn) return;
            const isModern = document.documentElement.classList.contains('theme-modern');
            btn.textContent = isModern ? 'クラシック' : 'モダン';
        }

        window.addEventListener('resize', () => {
            // ビューポート由来のデフォルトを再計算してマージ
            const table = document.getElementById('clients-table');
            if (table) {
                const totalCols = table.querySelectorAll('thead th').length;
                const savedPrefs = loadClientColumnPrefs();
                const defaults = getDefaultColumnVisibilityForClients(totalCols);
                clientsColumnVisibility = Object.assign({}, defaults, savedPrefs);
                // ピッカーのチェック状態を同期
                document.querySelectorAll('#clients-column-picker input[type="checkbox"][data-col]').forEach(chk => {
                    const col = parseInt(chk.dataset.col, 10);
                    if (!chk.disabled) chk.checked = !!clientsColumnVisibility[col];
                });
            }
            applyClientColumnVisibility();
            applyChannelColumnVisibility();
        });
    </script>
</div><!-- /.tabs-body -->
</div><!-- /.main-layout -->
</div><!-- /.container -->
</body>
</html>
"#;

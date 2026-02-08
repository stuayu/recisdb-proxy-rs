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
    <style>
        * { margin: 0; padding: 0; box-sizing: border-box; }

        body {
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            min-height: 100vh;
            padding: 20px;
        }

        .container { max-width: 1400px; margin: 0 auto; }

        header {
            background: rgba(255, 255, 255, 0.95);
            padding: 15px 20px;
            border-radius: 8px 8px 0 0;
            box-shadow: 0 2px 10px rgba(0, 0, 0, 0.1);
            display: flex;
            justify-content: space-between;
            align-items: center;
        }

        h1 { color: #333; font-size: 24px; }
        .subtitle { color: #666; font-size: 13px; }

        /* Tab Navigation */
        .tabs {
            display: flex;
            background: rgba(255, 255, 255, 0.9);
            border-bottom: 2px solid #667eea;
        }

        .tab {
            padding: 12px 24px;
            cursor: pointer;
            color: #666;
            font-weight: 500;
            border: none;
            background: none;
            font-size: 14px;
            transition: all 0.2s;
        }

        .tab:hover { color: #667eea; background: rgba(102, 126, 234, 0.1); }
        .tab.active { color: #667eea; background: white; border-bottom: 2px solid #667eea; margin-bottom: -2px; }

        /* Tab Content */
        .tab-content { display: none; background: white; padding: 20px; border-radius: 0 0 8px 8px; box-shadow: 0 2px 10px rgba(0, 0, 0, 0.1); }
        .tab-content.active { display: block; }

        /* Stats Grid */
        .stats-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(200px, 1fr)); gap: 15px; margin-bottom: 20px; }
        .stat-card { background: #f8f9fa; padding: 15px; border-radius: 8px; text-align: center; }
        .stat-label { color: #666; font-size: 11px; text-transform: uppercase; letter-spacing: 1px; margin-bottom: 5px; }
        .stat-value { color: #333; font-size: 24px; font-weight: bold; }

        /* Tables */
        table { width: 100%; border-collapse: collapse; }
        th { background: #f5f5f5; padding: 10px 12px; text-align: left; font-weight: 600; color: #333; border-bottom: 2px solid #ddd; font-size: 13px; }
        td { padding: 10px 12px; border-bottom: 1px solid #eee; color: #555; font-size: 13px; }
        tr:hover { background: #f9f9f9; }
        code { background: #f0f0f0; padding: 2px 6px; border-radius: 3px; font-size: 12px; }

        /* Performance graphs */
        .performance-graphs { display: flex; gap: 12px; flex-wrap: wrap; }
        .graph-container { background: #f8f9fa; padding: 10px 12px; border-radius: 8px; flex: 1; min-width: 220px; }
        .graph-container h4 { font-size: 12px; color: #666; margin-bottom: 6px; }
        .sparkline { width: 100%; height: 70px; }

        /* Buttons */
        .btn { display: inline-block; padding: 6px 12px; border: none; border-radius: 4px; cursor: pointer; font-size: 12px; transition: all 0.2s; }
        .btn-primary { background: #667eea; color: white; }
        .btn-primary:hover { background: #5a6fd6; }
        .btn-secondary { background: #6c757d; color: white; }
        .btn-secondary:hover { background: #5a6268; }
        .btn-success { background: #28a745; color: white; }
        .btn-success:hover { background: #218838; }
        .btn-danger { background: #dc3545; color: white; }
        .btn-danger:hover { background: #c82333; }
        .btn-warning { background: #ffc107; color: #333; }
        .btn-warning:hover { background: #e0a800; }
        .btn-sm { padding: 4px 8px; font-size: 11px; }

        /* Status Badges */
        .badge { display: inline-block; padding: 3px 10px; border-radius: 20px; font-size: 11px; font-weight: 600; }
        .badge-success { background: #d4edda; color: #155724; }
        .badge-danger { background: #f8d7da; color: #721c24; }
        .badge-warning { background: #fff3cd; color: #856404; }
        .badge-info { background: #d1ecf1; color: #0c5460; }

        /* Modal */
        .modal { display: none; position: fixed; z-index: 1000; left: 0; top: 0; width: 100%; height: 100%; background: rgba(0, 0, 0, 0.5); }
        .modal.active { display: flex; align-items: center; justify-content: center; }
        .modal-content { background: white; padding: 25px; border-radius: 8px; box-shadow: 0 5px 20px rgba(0, 0, 0, 0.3); max-width: 550px; width: 90%; max-height: 80vh; overflow-y: auto; }
        .modal h3 { color: #333; margin-bottom: 20px; font-size: 18px; }

        /* Form Elements */
        .form-group { margin-bottom: 15px; }
        .form-group label { display: block; color: #333; margin-bottom: 5px; font-weight: 500; font-size: 13px; }
        .form-group input, .form-group select { width: 100%; padding: 8px 12px; border: 1px solid #ddd; border-radius: 4px; font-size: 13px; }
        .form-group input:focus, .form-group select:focus { border-color: #667eea; outline: none; }
        .form-group input[readonly] { background: #f5f5f5; }
        .form-group small { display: block; color: #999; font-size: 12px; margin-top: 4px; }
        .form-check { display: flex; align-items: center; gap: 8px; }
        .form-check input[type="checkbox"] { width: auto; }

        .settings-form { max-width: 600px; }
        .settings-form .form-group { margin-bottom: 20px; }

        .form-actions { display: flex; justify-content: flex-end; gap: 10px; margin-top: 20px; padding-top: 15px; border-top: 1px solid #eee; }

        /* Section Header */
        .section-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 15px; }
        .section-header h3 { color: #333; font-size: 16px; }

        /* Empty State */
        .empty-state { text-align: center; padding: 40px; color: #999; }

        /* Toggle Switch */
        .toggle { position: relative; display: inline-block; width: 40px; height: 22px; }
        .toggle input { opacity: 0; width: 0; height: 0; }
        .toggle-slider { position: absolute; cursor: pointer; top: 0; left: 0; right: 0; bottom: 0; background: #ccc; border-radius: 22px; transition: 0.3s; }
        .toggle-slider:before { position: absolute; content: ""; height: 16px; width: 16px; left: 3px; bottom: 3px; background: white; border-radius: 50%; transition: 0.3s; }
        .toggle input:checked + .toggle-slider { background: #667eea; }
        .toggle input:checked + .toggle-slider:before { transform: translateX(18px); }

        /* Filter Bar */
        .filter-bar { display: flex; gap: 10px; margin-bottom: 15px; flex-wrap: wrap; align-items: center; }
        .filter-bar select, .filter-bar input { padding: 6px 10px; border: 1px solid #ddd; border-radius: 4px; font-size: 13px; }

        /* Loading */
        .loading { text-align: center; padding: 20px; color: #666; }

        /* Sortable headers */
        th.sortable { cursor: pointer; user-select: none; position: relative; padding-right: 20px; }
        th.sortable:hover { background: #e8e8e8; }
        th.sortable::after { content: '⇅'; position: absolute; right: 6px; opacity: 0.3; font-size: 10px; }
        th.sortable.asc::after { content: '▲'; opacity: 1; }
        th.sortable.desc::after { content: '▼'; opacity: 1; }

        .sort-bar { display: flex; gap: 10px; align-items: center; margin: 8px 0 12px; flex-wrap: wrap; }
        .sort-bar label { color: #666; font-size: 12px; }
        .mobile-only { display: none; }

        @media (max-width: 768px) {
            .stats-grid { grid-template-columns: repeat(2, 1fr); }
            .tabs { flex-wrap: wrap; }
            .tab { flex: 1; min-width: 80px; text-align: center; padding: 10px; font-size: 12px; }
            h1 { font-size: 18px; }

            .mobile-only { display: flex; }

            .responsive-table thead { display: none; }
            .responsive-table, .responsive-table tbody, .responsive-table tr, .responsive-table td { display: block; width: 100%; }
            .responsive-table tr { background: #fff; border: 1px solid #eee; border-radius: 8px; margin-bottom: 10px; overflow: hidden; }
            .responsive-table td { display: flex; justify-content: space-between; align-items: center; gap: 10px; padding: 8px 12px; border-bottom: 1px solid #f0f0f0; text-align: right; }
            .responsive-table td::before { content: attr(data-label); flex: 0 0 40%; color: #666; font-size: 11px; font-weight: 600; text-align: left; }
            .responsive-table td:last-child { border-bottom: none; }
        }
    </style>
</head>
<body>
    <div class="container">
        <header>
            <div>
                <h1>recisdb-proxy</h1>
                <p class="subtitle">TV Proxy Server Dashboard</p>
            </div>
            <div id="connection-status">
                <span class="badge badge-success">Connected</span>
            </div>
        </header>

        <nav class="tabs">
            <button class="tab active" data-tab="overview">概要</button>
            <button class="tab" data-tab="bondrivers">BonDriver</button>
            <button class="tab" data-tab="channels">チャンネル</button>
            <button class="tab" data-tab="scan-history">スキャン履歴</button>
            <button class="tab" data-tab="session-history">セッション履歴</button>
            <button class="tab" data-tab="alerts">アラート</button>
            <button class="tab" data-tab="settings">設定</button>
        </nav>

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
            <table id="clients-table" class="responsive-table sortable-table">
                <thead>
                    <tr>
                        <th class="sortable" data-sort-type="number">セッションID</th>
                        <th class="sortable" data-sort-type="text">クライアント</th>
                        <th class="sortable" data-sort-type="text">ホスト名</th>
                        <th class="sortable" data-sort-type="text">状態</th>
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
                        <th>操作</th>
                    </tr>
                </thead>
                <tbody id="clients-body">
                    <tr><td colspan="14" class="empty-state">接続中のクライアントはありません</td></tr>
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
            </div>
        </div>

        <!-- BonDriver Tab -->
        <div id="bondrivers" class="tab-content">
            <div class="section-header">
                <h3>BonDriver 一覧</h3>
                <button class="btn btn-secondary btn-sm" onclick="refreshBonDrivers()">更新</button>
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
                <div style="display: flex; gap: 10px;">
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
                </div>
            </div>
            <div class="sort-bar mobile-only">
                <label for="channel-sort-key">並び替え</label>
                <select id="channel-sort-key" onchange="setChannelSortFromUI()">
                    <option value="is_enabled">有効</option>
                    <option value="channel_name">チャンネル名</option>
                    <option value="nid">NID/SID/TSID</option>
                    <option value="band_type">バンド</option>
                    <option value="terrestrial_region">地域</option>
                    <option value="network_name">ネットワーク</option>
                    <option value="tuner_count">チューナー</option>
                    <option value="priority">優先度</option>
                </select>
                <button class="btn btn-secondary btn-sm" id="channel-sort-order" onclick="toggleChannelSortOrder()">昇順</button>
            </div>
            <table id="channels-table" class="responsive-table">
                <thead>
                    <tr>
                        <th class="sortable" data-sort="is_enabled">有効</th>
                        <th class="sortable" data-sort="channel_name">チャンネル名</th>
                        <th class="sortable" data-sort="nid">NID/SID/TSID</th>
                        <th class="sortable" data-sort="band_type">バンド</th>
                        <th class="sortable" data-sort="terrestrial_region">地域</th>
                        <th class="sortable" data-sort="network_name">ネットワーク</th>
                        <th class="sortable" data-sort="tuner_count">チューナー</th>
                        <th class="sortable" data-sort="priority">優先度</th>
                        <th>操作</th>
                    </tr>
                </thead>
                <tbody id="channels-body">
                    <tr><td colspan="9" class="loading">読み込み中...</td></tr>
                </tbody>
            </table>
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

                <div style="margin-top: 20px; display: flex; gap: 10px;">
                    <button class="btn btn-primary" onclick="saveTunerConfig()">保存</button>
                    <button class="btn btn-secondary" onclick="loadTunerConfig()">リセット</button>
                </div>

                <div id="tuner-config-message" style="margin-top: 15px; display: none;"></div>
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
    </div>

    <!-- BonDriver Edit Modal -->
    <div class="modal" id="bondriver-modal">
        <div class="modal-content">
            <h3>BonDriver 設定編集</h3>
            <form id="bondriver-form">
                <input type="hidden" id="bd-id">
                <div class="form-group">
                    <label>DLLパス</label>
                    <input type="text" id="bd-path" readonly>
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

        function enableTableSorting(tableId) {
            const table = document.getElementById(tableId);
            if (!table) return;
            const headers = Array.from(table.querySelectorAll('thead th.sortable'));
            headers.forEach((th, index) => {
                th.addEventListener('click', () => {
                    const type = th.dataset.sortType || 'text';
                    const isAsc = !th.classList.contains('asc');
                    headers.forEach(h => h.classList.remove('asc', 'desc'));
                    th.classList.add(isAsc ? 'asc' : 'desc');

                    const tbody = table.querySelector('tbody');
                    if (!tbody) return;
                    const rows = Array.from(tbody.querySelectorAll('tr')).filter(r => !r.querySelector('.empty-state') && !r.querySelector('.loading'));
                    rows.sort((a, b) => {
                        const aCell = a.children[index];
                        const bCell = b.children[index];
                        const aVal = aCell?.dataset.sortValue ?? aCell?.textContent ?? '';
                        const bVal = bCell?.dataset.sortValue ?? bCell?.textContent ?? '';
                        const va = parseSortValue(aVal, type);
                        const vb = parseSortValue(bVal, type);
                        if (va < vb) return isAsc ? -1 : 1;
                        if (va > vb) return isAsc ? 1 : -1;
                        return 0;
                    });
                    rows.forEach(row => tbody.appendChild(row));
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

        // BandType: 0=Terrestrial, 1=BS, 2=CS, 3=4K, 4=Other, 5=CATV, 6=SKY
        function getBandTypeName(bandType) {
            const names = ['地デジ', 'BS', 'CS', 'BS4K', 'その他', 'CATV', 'SKY'];
            return bandType !== null && bandType !== undefined ? (names[bandType] || '不明') : '-';
        }

        function getBandBadgeClass(bandType) {
            const classes = ['badge-success', 'badge-info', 'badge-warning', 'badge-info', 'badge-danger', 'badge-warning', 'badge-info'];
            return bandType !== null && bandType !== undefined ? (classes[bandType] || 'badge-danger') : '';
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
                    tbody.innerHTML = '<tr><td colspan="14" class="empty-state">接続中のクライアントはありません</td></tr>';
                    applyResponsiveLabels('clients-table');
                    return;
                }

                tbody.innerHTML = data.clients.map(c => `
                    <tr onclick="selectClient(${c.session_id})" style="cursor:pointer;">
                        <td data-sort-value="${c.session_id}">${c.session_id}</td>
                        <td data-sort-value="${escapeHtml(c.address)}">${escapeHtml(c.address)} <span style="color:#999;font-size:11px">(${formatDuration(c.connected_seconds)})</span></td>
                        <td data-sort-value="${escapeHtml(c.host || '-')}">${escapeHtml(c.host || '-')}</td>
                        <td data-sort-value="${c.is_streaming ? '1' : '0'}"><span class="badge ${c.is_streaming ? 'badge-success' : 'badge-warning'}">${c.is_streaming ? 'ストリーミング中' : '待機中'}</span></td>
                        <td data-sort-value="${escapeHtml(c.channel_name || c.channel_info || '-')}">${escapeHtml(c.channel_name || c.channel_info || '-')}</td>
                        <td data-sort-value="${c.signal_level || 0}">${c.signal_level || '-'} dB</td>
                        <td data-sort-value="${c.packets_sent || 0}">${formatPackets(c.packets_sent)}</td>
                        <td data-sort-value="${c.packets_dropped || 0}">${formatPackets(c.packets_dropped)}</td>
                        <td data-sort-value="${c.packets_scrambled || 0}">${formatPackets(c.packets_scrambled)}</td>
                        <td data-sort-value="${c.packets_error || 0}">${formatPackets(c.packets_error)}</td>
                        <td data-sort-value="${c.current_bitrate_mbps || 0}">${c.current_bitrate_mbps || '-'} Mbps</td>
                        <td data-sort-value="${c.effective_priority !== null && c.effective_priority !== undefined ? c.effective_priority : -99999}">${c.effective_priority !== null && c.effective_priority !== undefined ? c.effective_priority : '-'}</td>
                        <td data-sort-value="${c.effective_exclusive ? '1' : '0'}"><span class="badge ${c.effective_exclusive ? 'badge-danger' : 'badge-success'}">${c.effective_exclusive ? 'ON' : 'OFF'}</span></td>
                        <td data-sort-value="${(c.override_priority !== null && c.override_priority !== undefined) || (c.override_exclusive !== null && c.override_exclusive !== undefined) ? '1' : '0'}">
                            ${renderOverrideBadge(c)}
                            <button class="btn btn-primary btn-sm" onclick="event.stopPropagation(); openOverrideModal(${c.session_id}, ${c.override_priority !== null && c.override_priority !== undefined ? c.override_priority : 'null'}, ${c.override_exclusive !== null && c.override_exclusive !== undefined ? c.override_exclusive : 'null'});">設定</button>
                            <button class="btn btn-secondary btn-sm" onclick="event.stopPropagation(); clearOverride(${c.session_id});">解除</button>
                        </td>
                        <td><button class="btn btn-danger btn-sm" onclick="event.stopPropagation(); disconnectClient(${c.session_id});">切断</button></td>
                    </tr>
                `).join('');
                applyResponsiveLabels('clients-table');
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
                            <button class="btn btn-primary btn-sm" onclick='editBonDriver(${JSON.stringify(d)})'>編集</button>
                            <button class="btn btn-warning btn-sm" onclick="triggerScan(${d.id})">スキャン</button>
                        </td>
                    </tr>
                `}).join('');
                applyResponsiveLabels('bondrivers-table');
            } catch (e) { console.error('Failed to refresh bondrivers:', e); }
        }

        function editBonDriver(d) {
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

        document.getElementById('bondriver-form').onsubmit = async (e) => {
            e.preventDefault();
            const id = document.getElementById('bd-id').value;
            try {
                const res = await fetch(`/api/bondriver/${id}`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({
                        driver_name: document.getElementById('bd-name').value || null,
                        group_name: document.getElementById('bd-group-name').value || null,
                        max_instances: parseInt(document.getElementById('bd-max-instances').value),
                        auto_scan_enabled: document.getElementById('bd-auto-scan').checked,
                        scan_interval_hours: parseInt(document.getElementById('bd-scan-interval').value),
                        scan_priority: parseInt(document.getElementById('bd-scan-priority').value),
                        passive_scan_enabled: document.getElementById('bd-passive-scan').checked
                    })
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
        let channelSortKey = 'nid';
        let channelSortAsc = true;

        function renderChannels() {
            const tbody = document.getElementById('channels-body');
            if (channelData.length === 0) {
                tbody.innerHTML = '<tr><td colspan="9" class="empty-state">チャンネルがありません</td></tr>';
                applyResponsiveLabels('channels-table');
                return;
            }

            // Sort the data
            const sorted = [...channelData].sort((a, b) => {
                let va = a[channelSortKey];
                let vb = b[channelSortKey];

                // Handle null/undefined
                if (va === null || va === undefined) va = '';
                if (vb === null || vb === undefined) vb = '';

                // Numeric comparison for number fields
                if (typeof va === 'number' && typeof vb === 'number') {
                    return channelSortAsc ? va - vb : vb - va;
                }
                // Boolean comparison
                if (typeof va === 'boolean') {
                    return channelSortAsc ? (va === vb ? 0 : va ? -1 : 1) : (va === vb ? 0 : va ? 1 : -1);
                }
                // String comparison
                const strA = String(va).toLowerCase();
                const strB = String(vb).toLowerCase();
                return channelSortAsc ? strA.localeCompare(strB, 'ja') : strB.localeCompare(strA, 'ja');
            });

            tbody.innerHTML = sorted.map(c => `
                <tr>
                    <td>
                        <label class="toggle">
                            <input type="checkbox" ${c.is_enabled ? 'checked' : ''} onchange="toggleChannel(${c.id}, this.checked)">
                            <span class="toggle-slider"></span>
                        </label>
                    </td>
                    <td>${escapeHtml(c.channel_name || c.raw_name || '-')}</td>
                    <td><code>0x${c.nid.toString(16).toUpperCase().padStart(4,'0')}/${c.sid}/${c.tsid}</code></td>
                    <td><span class="badge ${getBandBadgeClass(c.band_type)}">${getBandTypeName(c.band_type)}</span></td>
                    <td>${escapeHtml(c.terrestrial_region || '-')}</td>
                    <td>${escapeHtml(c.network_name || '-')}</td>
                    <td>${c.tuner_count ? `<span class="badge badge-info" title="${escapeHtml((c.tuner_names || []).join(', '))}">${c.tuner_count}台</span>` : (c.bon_space !== null ? c.bon_space + '/' + c.bon_channel : '-')}</td>
                    <td>${c.priority}</td>
                    <td>
                        <button class="btn btn-primary btn-sm" onclick='editChannel(${JSON.stringify(c)})'>編集</button>
                    </td>
                </tr>
            `).join('');
            applyResponsiveLabels('channels-table');
        }

        function sortChannels(key) {
            if (channelSortKey === key) {
                channelSortAsc = !channelSortAsc;
            } else {
                channelSortKey = key;
                channelSortAsc = true;
            }
            updateChannelSortIndicators();
            updateChannelSortUI();
            renderChannels();
        }

        function updateChannelSortIndicators() {
            document.querySelectorAll('#channels-table th.sortable').forEach(th => {
                th.classList.remove('asc', 'desc');
                if (th.dataset.sort === channelSortKey) {
                    th.classList.add(channelSortAsc ? 'asc' : 'desc');
                }
            });
        }

        function updateChannelSortUI() {
            const select = document.getElementById('channel-sort-key');
            const orderBtn = document.getElementById('channel-sort-order');
            if (select) select.value = channelSortKey;
            if (orderBtn) orderBtn.textContent = channelSortAsc ? '昇順' : '降順';
        }

        function setChannelSortFromUI() {
            const select = document.getElementById('channel-sort-key');
            if (!select) return;
            channelSortKey = select.value;
            channelSortAsc = true;
            updateChannelSortIndicators();
            updateChannelSortUI();
            renderChannels();
        }

        function toggleChannelSortOrder() {
            channelSortAsc = !channelSortAsc;
            updateChannelSortIndicators();
            updateChannelSortUI();
            renderChannels();
        }

        // Add click handlers to sortable headers
        document.querySelectorAll('#channels-table th.sortable').forEach(th => {
            th.addEventListener('click', () => sortChannels(th.dataset.sort));
        });

        async function refreshChannels() {
            try {
                const bondriverId = document.getElementById('channel-bondriver-filter').value;
                const groupLogical = document.getElementById('channel-group-filter').checked;
                const enabledOnly = document.getElementById('channel-enabled-filter').checked;

                let url = '/api/channels?';
                if (bondriverId) url += `bondriver_id=${bondriverId}&`;
                if (groupLogical && !bondriverId) url += 'group_logical=true&';
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

        function editChannel(c) {
            document.getElementById('ch-id').value = c.id;
            document.getElementById('ch-info').value = `NID:${c.nid} SID:${c.sid} TSID:${c.tsid}`;
            document.getElementById('ch-name').value = c.channel_name || '';
            document.getElementById('ch-priority').value = c.priority;
            document.getElementById('ch-enabled').checked = c.is_enabled;
            openModal('channel-modal');
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
                    hideConfigMessage();
                }
            } catch (e) { console.error('Failed to load scan config:', e); }
        }

        async function saveScanConfig() {
            const config = {
                check_interval_secs: parseInt(document.getElementById('check-interval').value),
                max_concurrent_scans: parseInt(document.getElementById('max-concurrent').value),
                scan_timeout_secs: parseInt(document.getElementById('scan-timeout').value)
            };

            if (config.check_interval_secs <= 0 || config.max_concurrent_scans <= 0 || config.scan_timeout_secs <= 0) {
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
                    hideTunerConfigMessage();
                }
            } catch (e) { console.error('Failed to load tuner config:', e); }
        }

        async function saveTunerConfig() {
            const config = {
                keep_alive_secs: parseInt(document.getElementById('tuner-keep-alive').value),
                prewarm_enabled: document.getElementById('tuner-prewarm-enabled').checked,
                prewarm_timeout_secs: parseInt(document.getElementById('tuner-prewarm-timeout').value)
            };

            if (config.keep_alive_secs < 0 || config.prewarm_timeout_secs <= 0) {
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

        // Initialize
        window.addEventListener('load', () => {
            refreshStats();
            refreshClients();
            loadScanConfig();
            loadTunerConfig();
            enableTableSorting('clients-table');
            enableTableSorting('bondrivers-table');
            enableTableSorting('history-table');
            enableTableSorting('session-history-table');
            enableTableSorting('alerts-table');
            enableTableSorting('alert-rules-table');
            setInterval(() => { refreshStats(); refreshClients(); updateClientMetrics(); }, 2000);
        });
    </script>
</body>
</html>
"#;

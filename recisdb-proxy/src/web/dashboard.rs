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

        @media (max-width: 768px) {
            .stats-grid { grid-template-columns: repeat(2, 1fr); }
            .tabs { flex-wrap: wrap; }
            .tab { flex: 1; min-width: 80px; text-align: center; padding: 10px; font-size: 12px; }
            h1 { font-size: 18px; }
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
            <button class="tab" data-tab="history">スキャン履歴</button>
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
            <table id="clients-table">
                <thead>
                    <tr>
                        <th>セッションID</th>
                        <th>クライアント</th>
                        <th>状態</th>
                        <th>チャンネル</th>
                        <th>信号レベル</th>
                        <th>送信パケット</th>
                    </tr>
                </thead>
                <tbody id="clients-body">
                    <tr><td colspan="6" class="empty-state">接続中のクライアントはありません</td></tr>
                </tbody>
            </table>
        </div>

        <!-- BonDriver Tab -->
        <div id="bondrivers" class="tab-content">
            <div class="section-header">
                <h3>BonDriver 一覧</h3>
                <button class="btn btn-secondary btn-sm" onclick="refreshBonDrivers()">更新</button>
            </div>
            <table id="bondrivers-table">
                <thead>
                    <tr>
                        <th>DLLパス</th>
                        <th>表示名</th>
                        <th>グループ名</th>
                        <th>最大インスタンス</th>
                        <th>自動スキャン</th>
                        <th>次回スキャン</th>
                        <th>操作</th>
                    </tr>
                </thead>
                <tbody id="bondrivers-body">
                    <tr><td colspan="7" class="loading">読み込み中...</td></tr>
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
            <table id="channels-table">
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
        </div>

        <!-- History Tab -->
        <div id="history" class="tab-content">
            <div class="section-header">
                <h3>スキャン履歴</h3>
                <button class="btn btn-secondary btn-sm" onclick="refreshHistory()">更新</button>
            </div>
            <table id="history-table">
                <thead>
                    <tr>
                        <th>日時</th>
                        <th>BonDriver ID</th>
                        <th>結果</th>
                        <th>チャンネル数</th>
                        <th>メッセージ</th>
                    </tr>
                </thead>
                <tbody id="history-body">
                    <tr><td colspan="5" class="loading">読み込み中...</td></tr>
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
                else if (tab.dataset.tab === 'history') refreshHistory();
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
                    document.getElementById('stat-sessions').textContent = stats.stats.total_sessions || 0;
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
                    tbody.innerHTML = '<tr><td colspan="6" class="empty-state">接続中のクライアントはありません</td></tr>';
                    return;
                }

                tbody.innerHTML = data.clients.map(c => `
                    <tr>
                        <td>${c.session_id}</td>
                        <td>${escapeHtml(c.address)} <span style="color:#999;font-size:11px">(${formatDuration(c.connected_seconds)})</span></td>
                        <td><span class="badge ${c.is_streaming ? 'badge-success' : 'badge-warning'}">${c.is_streaming ? 'ストリーミング中' : '待機中'}</span></td>
                        <td title="${escapeHtml(c.tuner_path || '')}">${escapeHtml(c.channel_name || c.channel_info || '-')}</td>
                        <td>${c.signal_level || '-'} dB</td>
                        <td>${formatPackets(c.packets_sent)}</td>
                    </tr>
                `).join('');
            } catch (e) { console.error('Failed to refresh clients:', e); }
        }

        // BonDrivers
        async function refreshBonDrivers() {
            try {
                const res = await fetch('/api/bondrivers');
                const data = await res.json();
                const tbody = document.getElementById('bondrivers-body');
                const filter = document.getElementById('channel-bondriver-filter');

                if (!data.success || !data.bondrivers) {
                    tbody.innerHTML = '<tr><td colspan="6" class="empty-state">BonDriverが登録されていません</td></tr>';
                    return;
                }

                // Update filter dropdown
                filter.innerHTML = '<option value="">すべてのBonDriver</option>' +
                    data.bondrivers.map(d => `<option value="${d.id}">${escapeHtml(d.driver_name || d.dll_path)}</option>`).join('');

                tbody.innerHTML = data.bondrivers.map(d => {
                    const nextScan = d.next_scan_at ? formatDateTime(d.next_scan_at) : '-';
                    return `
                    <tr>
                        <td><code>${escapeHtml(d.dll_path)}</code></td>
                        <td>${escapeHtml(d.driver_name) || '-'}</td>
                        <td>${escapeHtml(d.group_name) || '-'}</td>
                        <td>${d.max_instances}</td>
                        <td><span class="badge ${d.auto_scan_enabled ? 'badge-success' : 'badge-danger'}">${d.auto_scan_enabled ? 'ON' : 'OFF'}</span></td>
                        <td>${nextScan}</td>
                        <td>
                            <button class="btn btn-primary btn-sm" onclick='editBonDriver(${JSON.stringify(d)})'>編集</button>
                            <button class="btn btn-warning btn-sm" onclick="triggerScan(${d.id})">スキャン</button>
                        </td>
                    </tr>
                `}).join('');
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
        }

        function sortChannels(key) {
            if (channelSortKey === key) {
                channelSortAsc = !channelSortAsc;
            } else {
                channelSortKey = key;
                channelSortAsc = true;
            }
            // Update header styles
            document.querySelectorAll('#channels-table th.sortable').forEach(th => {
                th.classList.remove('asc', 'desc');
                if (th.dataset.sort === key) {
                    th.classList.add(channelSortAsc ? 'asc' : 'desc');
                }
            });
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
                    return;
                }

                tbody.innerHTML = data.history.map(h => `
                    <tr>
                        <td>${formatDateTime(h.scan_time)}</td>
                        <td>${h.bon_driver_id}</td>
                        <td><span class="badge ${h.success ? 'badge-success' : 'badge-danger'}">${h.success ? '成功' : '失敗'}</span></td>
                        <td>${h.channel_count !== null ? h.channel_count : '-'}</td>
                        <td>${escapeHtml(h.error_message) || '-'}</td>
                    </tr>
                `).join('');
            } catch (e) { console.error('Failed to refresh history:', e); }
        }

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

        // Initialize
        window.addEventListener('load', () => {
            refreshStats();
            refreshClients();
            loadScanConfig();
            setInterval(() => { refreshStats(); refreshClients(); }, 2000);
        });
    </script>
</body>
</html>
"#;

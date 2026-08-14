<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, type JsonRecord } from '../api'
import { useDashboardStore } from '../stores/dashboard'
import MetricsChart from './MetricsChart.vue'
import PreviewPlayer from './PreviewPlayer.vue'
const store = useDashboardStore()
const selectedSession = ref<string | number>('')
const previewRow = ref<JsonRecord | null>(null)
const previewSid = computed<string | number>(() => {
  const value = previewRow.value?.sid
  return typeof value === 'number' || typeof value === 'string' ? value : ''
})
function openPreview(row: JsonRecord) {
  if (row.sid == null) return
  previewRow.value = row
}
function closePreview() {
  previewRow.value = null
}

type ClientColumn = { key: string; label: string }
const clientColumns: ClientColumn[] = [
  { key: 'session_id', label: 'セッションID' },
  { key: 'protocol', label: '接続方式' },
  { key: 'address', label: 'クライアント' },
  { key: 'host', label: 'ホスト名' },
  { key: 'status', label: '状態' },
  { key: 'tuner_path', label: '選択チューナー' },
  { key: 'channel', label: 'チャンネル' },
  { key: 'signal', label: '信号' },
  { key: 'packets_sent', label: '送信' },
  { key: 'packets_dropped', label: 'Drop' },
  { key: 'packets_scrambled', label: 'Scramble' },
  { key: 'packets_error', label: 'Error' },
  { key: 'bitrate', label: 'ビットレート' },
  { key: 'stream_class', label: 'クラス' },
  { key: 'prefilling', label: 'プリフィル' },
]
const clientColumnsStorageKey = 'clientColumns'
function initialClientKeys(): string[] {
  const saved = localStorage.getItem(clientColumnsStorageKey)
  if (saved) {
    try {
      const parsed: unknown = JSON.parse(saved)
      if (Array.isArray(parsed)) {
        const valid = parsed.filter(
          (key): key is string =>
            typeof key === 'string' && clientColumns.some((column) => column.key === key),
        )
        // 接続方式は BNDP と HTTP(EPGStation 等)を見分ける唯一の列なので、
        // 列構成を保存済みの利用者にも必ず出す(保存値には無い新しい列)。
        if (valid.length) return valid.includes('protocol') ? valid : ['session_id', 'protocol', ...valid.filter((key) => key !== 'session_id')]
      }
    } catch {
      localStorage.removeItem(clientColumnsStorageKey)
    }
  }
  return clientColumns.map((column) => column.key)
}
const visibleClientKeys = ref<string[]>(initialClientKeys())
const visibleClientColumns = computed(() =>
  clientColumns.filter((column) => visibleClientKeys.value.includes(column.key)),
)
function setClientColumn(key: string, checked: boolean) {
  const next = new Set(visibleClientKeys.value)
  if (checked) next.add(key)
  else next.delete(key)
  visibleClientKeys.value = clientColumns
    .map((column) => column.key)
    .filter((columnKey) => next.has(columnKey))
  localStorage.setItem(clientColumnsStorageKey, JSON.stringify(visibleClientKeys.value))
}
function resetClientColumns() {
  visibleClientKeys.value = clientColumns.map((column) => column.key)
  localStorage.removeItem(clientColumnsStorageKey)
}
// 生値のまま出すと 12.34567890123 のように桁が伸びるので、必ず桁数を丸める。
function formatNumber(value: unknown, digits: number): string {
  const num = typeof value === 'number' ? value : Number(value)
  return Number.isFinite(num) ? num.toFixed(digits) : String(value)
}
function cellText(row: JsonRecord, key: string): string {
  switch (key) {
    case 'protocol':
      // BNDP は TVTest/EDCB、mirakurun は EPGStation などの録画クライアント、
      // http はダッシュボードのプレビュー。
      return (
        { bndp: 'BonDriver', http: 'HTTP', mirakurun: 'Mirakurun' }[String(row.protocol ?? '')] ??
        '—'
      )
    case 'status':
      return row.is_streaming ? '配信中' : '接続中'
    case 'channel':
      return String(row.channel_name ?? row.channel_info ?? '—')
    case 'signal':
      return row.signal_level == null ? '—' : `${formatNumber(row.signal_level, 1)} dB`
    case 'packets_sent':
    case 'packets_dropped':
    case 'packets_scrambled':
    case 'packets_error':
      return String(row[key] ?? 0)
    case 'bitrate':
      return row.current_bitrate_mbps == null
        ? '—'
        : `${formatNumber(row.current_bitrate_mbps, 2)} Mbps`
    case 'prefilling':
      return row.prefilling ? '中' : '—'
    default: {
      const value = row[key]
      return value == null || value === '' ? '—' : String(value)
    }
  }
}

const tunerSummary = computed(() => {
  const active = store.stats.active_tuners
  if (active === undefined || active === null) return '—'
  const scanning = Number(store.stats.scanning_tuners ?? 0)
  return scanning > 0 ? `${active} (+スキャン ${scanning})` : String(active)
})
const cards = computed(() => [
  // スキャンもチューナー枠を1つ占有するので、視聴中のチューナーとは別に
  // 見えるようにしておく (合計が max_instances に対して何本埋まっているかを
  // 利用者が把握できるようにするため)。
  ['アクティブチューナー', tunerSummary.value],
  ['接続クライアント', store.stats.active_sessions ?? store.clients.length],
  ['総セッション', store.stats.total_sessions ?? '—'],
  ['登録チャンネル', store.stats.total_channels ?? '—'],
])
async function disconnect(row: JsonRecord) {
  if (!confirm('このクライアントを切断しますか？')) return
  await api(`/client/${row.session_id}/disconnect`, { method: 'POST' })
  if (selectedSession.value === row.session_id) selectedSession.value = ''
  await store.refresh()
}
async function setPriority(row: JsonRecord, event: Event) {
  const value = (event.target as HTMLSelectElement).value
  await api(`/client/${row.session_id}/controls`, {
    method: 'POST',
    body: JSON.stringify({ override_priority: value === '' ? null : Number(value) }),
  })
  await store.refresh()
}
async function setExclusive(row: JsonRecord, event: Event) {
  const value = (event.target as HTMLSelectElement).value
  await api(`/client/${row.session_id}/controls`, {
    method: 'POST',
    body: JSON.stringify({ override_exclusive: value === '' ? null : value === 'true' }),
  })
  await store.refresh()
}
onMounted(() => store.start())
onUnmounted(() => store.stop())
</script>
<template>
  <section class="view">
    <div class="view-heading">
      <div>
        <h2>概要</h2>
        <p>チューナーとクライアントの現在状態</p>
      </div>
      <button class="button" :disabled="store.loading" @click="store.refresh">更新</button>
    </div>
    <p v-if="store.error" class="notice error" role="alert" v-text="store.error" />
    <div class="stat-grid">
      <article v-for="card in cards" :key="String(card[0])" class="stat-card">
        <span v-text="card[0]" /><strong v-text="card[1]" />
      </article>
    </div>
    <h3>接続中のクライアント</h3>
    <details class="column-picker">
      <summary><span v-text="`表示列を調整（${visibleClientColumns.length}列）`" /></summary>
      <div class="column-options">
        <label v-for="column in clientColumns" :key="column.key" class="check compact-check">
          <input
            type="checkbox"
            :checked="visibleClientKeys.includes(column.key)"
            @change="setClientColumn(column.key, ($event.target as HTMLInputElement).checked)"
          />
          <span v-text="column.label" />
        </label>
      </div>
      <button class="button small secondary" @click="resetClientColumns">既定に戻す</button>
    </details>
    <div class="table-region">
      <table v-if="store.clients.length" class="data-table">
        <thead>
          <tr>
            <th v-for="column in visibleClientColumns" :key="column.key" v-text="column.label" />
            <th>優先度</th>
            <th>排他</th>
            <th>操作</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="row in store.clients" :key="String(row.session_id)">
            <td
              v-for="column in visibleClientColumns"
              :key="column.key"
              :data-label="column.label"
              v-text="cellText(row, column.key)"
            />
            <td data-label="優先度">
              <select :value="row.override_priority ?? ''" @change="setPriority(row, $event)">
                <option value="">自動</option>
                <option v-for="n in [1, 2, 3, 4, 5]" :key="n" :value="n" v-text="n" />
              </select>
            </td>
            <td data-label="排他">
              <select
                :value="row.override_exclusive == null ? '' : String(row.override_exclusive)"
                @change="setExclusive(row, $event)"
              >
                <option value="">自動</option>
                <option value="true">有効</option>
                <option value="false">無効</option>
              </select>
            </td>
            <td data-label="操作">
              <div class="actions">
                <button
                  class="button small secondary"
                  @click="selectedSession = String(row.session_id)"
                >
                  グラフ</button
                ><button
                  class="button small secondary"
                  :disabled="row.sid == null"
                  :title="row.sid == null ? '未選局のためプレビューできません' : ''"
                  @click="openPreview(row)"
                >
                  プレビュー</button
                ><button class="button small danger" @click="disconnect(row)">切断</button>
              </div>
            </td>
          </tr>
        </tbody>
      </table>
      <p v-else class="empty-state">接続中のクライアントはありません</p>
    </div>
    <MetricsChart v-if="selectedSession" :session-id="selectedSession" />
    <div v-if="previewRow" class="dialog-backdrop" @click.self="closePreview">
      <section
        class="dialog preview-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="client-preview-title"
      >
        <div class="view-heading">
          <div>
            <h2 id="client-preview-title">クライアントプレビュー</h2>
            <p
              class="muted"
              v-text="
                `${String(previewRow.channel_name ?? previewRow.channel_info ?? 'SID ' + previewRow.sid)}（セッション ${String(previewRow.session_id)}）`
              "
            />
          </div>
          <button class="button secondary" @click="closePreview">閉じる</button>
        </div>
        <PreviewPlayer :key="previewSid" :initial-sid="previewSid" />
      </section>
    </div>
  </section>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, type JsonRecord } from '../api'
import { useDashboardStore } from '../stores/dashboard'
import MetricsChart from './MetricsChart.vue'
import MetricSparkline from './MetricSparkline.vue'
import PreviewPlayer from './PreviewPlayer.vue'
const store = useDashboardStore()
const selectedSession = ref<string | number>('')
const previewRow = ref<JsonRecord | null>(null)
const previewSid = computed<string | number>(() => {
  const value = previewRow.value?.sid
  return typeof value === 'number' || typeof value === 'string' ? value : ''
})
const previewNid = computed<string | number | null>(() => {
  const value = previewRow.value?.nid
  return typeof value === 'number' || typeof value === 'string' ? value : null
})
const systemMetrics = ref<JsonRecord>({})
const systemHistory = ref<JsonRecord>({})
let systemTimer = 0
let systemChartWidth = 320

function record(value: unknown): JsonRecord {
  return value && typeof value === 'object' ? (value as JsonRecord) : {}
}
const systemCpu = computed(() => record(systemMetrics.value.cpu))
const systemMemory = computed(() => record(systemMetrics.value.memory))
const systemNetwork = computed(() => record(systemMetrics.value.network))
const systemGpus = computed(() => {
  const gpus = systemMetrics.value.gpus
  return Array.isArray(gpus) ? gpus.filter((gpu): gpu is JsonRecord => !!gpu && typeof gpu === 'object') : []
})
const systemGpuHistory = computed(() =>
  Array.isArray(systemHistory.value.gpu_usage)
    ? systemHistory.value.gpu_usage.filter((gpu): gpu is JsonRecord => !!gpu && typeof gpu === 'object')
    : [],
)
function gpuHistory(gpu: JsonRecord): unknown {
  return systemGpuHistory.value.find(
    (item) => item.index === gpu.index && item.vendor === gpu.vendor,
  )?.values
}
function formatPercent(value: unknown): string {
  const number = Number(value)
  return value == null || !Number.isFinite(number) ? '使用率 未取得' : `${number.toFixed(1)}%`
}
function formatBytes(value: unknown): string {
  let bytes = Number(value)
  if (!Number.isFinite(bytes)) return '—'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let index = 0
  while (bytes >= 1024 && index < units.length - 1) {
    bytes /= 1024
    index += 1
  }
  return `${bytes.toFixed(index === 0 ? 0 : 1)} ${units[index]}`
}
function formatRate(value: unknown): string {
  const rate = Number(value)
  if (!Number.isFinite(rate)) return '—'
  if (rate < 1000) return `${rate.toFixed(0)} bps`
  if (rate < 1_000_000) return `${(rate / 1000).toFixed(1)} Kbps`
  if (rate < 1_000_000_000) return `${(rate / 1_000_000).toFixed(1)} Mbps`
  return `${(rate / 1_000_000_000).toFixed(1)} Gbps`
}
async function loadSystemMetrics() {
  try {
    const [current, history] = await Promise.all([
      api<JsonRecord>('/system/metrics'),
      api<JsonRecord>('/system/metrics-history'),
    ])
    if (current.current === null) return
    systemMetrics.value = current
    systemHistory.value = history
  } catch {
    // システムメトリクスは概要画面の他の状態を隠さない。
  }
}
function startSystemMetrics() {
  window.clearInterval(systemTimer)
  void loadSystemMetrics()
  systemTimer = window.setInterval(loadSystemMetrics, 5000)
}
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
onMounted(() => {
  store.start()
  startSystemMetrics()
})
onUnmounted(() => {
  store.stop()
  window.clearInterval(systemTimer)
})
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
    <section class="metrics-panel system-metrics-panel">
      <div class="view-heading">
        <div>
          <h3>システム負荷</h3>
          <p>ホスト全体・5秒ごとに更新（過去15分）</p>
        </div>
      </div>
      <div class="metric-grid">
        <article>
          <strong>CPU</strong>
          <div class="system-metric-value">
            {{ Number.isFinite(Number(systemCpu.usage_percent)) ? `${Number(systemCpu.usage_percent).toFixed(1)}%` : '—' }}
          </div>
          <span class="muted">{{ systemCpu.cores ?? '—' }} コア / load {{ systemCpu.load_average_1 ?? '—' }}</span>
          <MetricSparkline :source="systemHistory.cpu_usage" unit="%" label="CPU使用率推移" :width="systemChartWidth" :height="90" />
        </article>
        <article>
          <strong>メモリ</strong>
          <div class="system-metric-value">{{ formatBytes(systemMemory.used_bytes) }}</div>
          <span class="muted">/ {{ formatBytes(systemMemory.total_bytes) }}</span>
          <MetricSparkline :source="systemHistory.memory_used" unit="bytes" label="メモリ使用量推移" :width="systemChartWidth" :height="90" />
        </article>
        <template v-if="systemGpus.length">
          <article v-for="gpu in systemGpus" :key="`${gpu.vendor}-${gpu.index}`">
            <strong>{{ gpu.vendor }} #{{ gpu.index }} {{ gpu.name }}</strong>
            <div class="system-metric-value">{{ formatPercent(gpu.usage_percent) }}</div>
            <span class="muted">
              VRAM:
              {{ gpu.memory_used_bytes == null ? '未取得' : formatBytes(gpu.memory_used_bytes) }} /
              {{ gpu.memory_total_bytes == null ? '未取得' : formatBytes(gpu.memory_total_bytes) }}
            </span>
            <details v-if="systemGpus.length >= 3" class="gpu-chart-details">
              <summary>グラフを表示</summary>
              <MetricSparkline :source="gpuHistory(gpu)" unit="%" :label="`${gpu.vendor} #${gpu.index} 使用率推移`" :width="systemChartWidth" :height="90" />
            </details>
            <MetricSparkline v-else :source="gpuHistory(gpu)" unit="%" :label="`${gpu.vendor} #${gpu.index} 使用率推移`" :width="systemChartWidth" :height="90" />
          </article>
        </template>
        <article>
          <strong>ネットワーク受信</strong>
          <div class="system-metric-value">{{ formatRate(systemNetwork.receive_bps) }}</div>
          <span class="muted">累計 {{ formatBytes(systemNetwork.received_bytes) }}</span>
          <MetricSparkline :source="systemHistory.network_receive_bps" unit="bps" label="ネットワーク受信速度推移" :width="systemChartWidth" :height="90" />
        </article>
        <article>
          <strong>ネットワーク送信</strong>
          <div class="system-metric-value">{{ formatRate(systemNetwork.transmit_bps) }}</div>
          <span class="muted">累計 {{ formatBytes(systemNetwork.transmitted_bytes) }}</span>
          <MetricSparkline :source="systemHistory.network_transmit_bps" unit="bps" label="ネットワーク送信速度推移" :width="systemChartWidth" :height="90" />
        </article>
      </div>
    </section>
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
        <PreviewPlayer :key="`${previewNid}-${previewSid}`" :initial-sid="previewSid" :initial-nid="previewNid" />
      </section>
    </div>
  </section>
</template>

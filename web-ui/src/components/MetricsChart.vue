<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { api, type JsonRecord } from '../api'

const props = defineProps<{ sessionId: string | number }>()
const data = ref<JsonRecord>({})
const quality = ref<JsonRecord>({})
const error = ref('')
const container = ref<HTMLElement | null>(null)
const chartWidth = ref(320)
const chartHeight = computed(() => (chartWidth.value < 360 ? 84 : 110))
let timer = 0
let observer: ResizeObserver | null = null

function values(source: unknown): number[] {
  if (!Array.isArray(source)) return []
  return source
    .map((value) => (Array.isArray(value) ? Number(value[1]) : Number(value)))
    .filter(Number.isFinite)
}

function points(source: unknown): string {
  const samples = values(source)
  if (!samples.length) return ''
  const width = Math.max(100, chartWidth.value)
  const height = chartHeight.value
  const padding = 6
  const min = Math.min(...samples)
  const max = Math.max(...samples)
  const range = max - min || 1
  return samples
    .map((value, index) => {
      const x = padding + (index / Math.max(1, samples.length - 1)) * (width - padding * 2)
      const y = height - padding - ((value - min) / range) * (height - padding * 2)
      return `${x.toFixed(1)},${y.toFixed(1)}`
    })
    .join(' ')
}

const bitrate = computed(() => points(data.value.bitrate))
const signal = computed(() => points(data.value.signal_level))
const loss = computed(() => points(data.value.packet_loss))
const viewBox = computed(() => `0 0 ${Math.max(100, chartWidth.value)} ${chartHeight.value}`)

async function load() {
  try {
    const [metrics, qualityData] = await Promise.all([
      api<JsonRecord>(`/client/${props.sessionId}/metrics-history`),
      api<JsonRecord>(`/client/${props.sessionId}/quality`),
    ])
    data.value = metrics
    quality.value = qualityData
    error.value = ''
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}

const topLossPids = computed(() =>
  Array.isArray(quality.value.top_loss_pids) ? quality.value.top_loss_pids : [],
)
function pidValue(item: unknown, key: string, position: number): unknown {
  if (Array.isArray(item)) return item[position]
  if (item && typeof item === 'object') {
    const record = item as JsonRecord
    return (
      record[key] ??
      (key === 'errors' ? record.cc_errors : key === 'packets' ? record.total : undefined)
    )
  }
  return undefined
}

function start() {
  window.clearInterval(timer)
  void load()
  timer = window.setInterval(load, 3000)
}

watch(() => props.sessionId, start)

onMounted(() => {
  start()
  if ('ResizeObserver' in window && container.value) {
    observer = new ResizeObserver(([entry]) => {
      chartWidth.value = Math.max(100, Math.floor(entry.contentRect.width))
    })
    observer.observe(container.value)
  }
})

onUnmounted(() => {
  window.clearInterval(timer)
  observer?.disconnect()
})
</script>

<template>
  <section ref="container" class="metrics-panel">
    <div class="view-heading">
      <div>
        <h3>リアルタイムメトリクス</h3>
        <p>3秒ごとに更新・画面幅へ自動追従</p>
      </div>
      <button class="button small secondary" @click="load">更新</button>
    </div>
    <div class="metric-grid">
      <article>
        <strong>ビットレート</strong>
        <svg :viewBox="viewBox" role="img" aria-label="ビットレート推移">
          <polyline :points="bitrate" />
        </svg>
      </article>
      <article>
        <strong>信号レベル</strong>
        <svg :viewBox="viewBox" role="img" aria-label="信号レベル推移">
          <polyline :points="signal" />
        </svg>
      </article>
      <article>
        <strong>パケット損失</strong>
        <svg :viewBox="viewBox" role="img" aria-label="パケット損失推移">
          <polyline :points="loss" />
        </svg>
      </article>
    </div>
    <section class="quality-panel">
      <h4>配信経路の損失（プロキシ内部）</h4>
      <div class="stat-grid compact-stats">
        <article class="stat-card">
          <span>broadcast lag（chunks）</span
          ><strong v-text="String(quality.loss_broadcast_lag_chunks ?? 0)" />
        </article>
        <article class="stat-card">
          <span>TS queue drop（chunks）</span
          ><strong v-text="String(quality.loss_ts_queue_chunks ?? 0)" />
        </article>
        <article class="stat-card">
          <span>encoder stall（events）</span
          ><strong v-text="String(quality.loss_encoder_stall_events ?? 0)" />
        </article>
      </div>
      <h4>ロス上位 PID（CC error）</h4>
      <div class="table-region">
        <table v-if="topLossPids.length" class="data-table compact">
          <thead>
            <tr>
              <th>PID</th>
              <th>パケット数</th>
              <th>CCエラー数</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="(pid, index) in topLossPids" :key="index">
              <td data-label="PID" v-text="String(pidValue(pid, 'pid', 0) ?? '—')" />
              <td data-label="パケット数" v-text="String(pidValue(pid, 'packets', 1) ?? '—')" />
              <td data-label="CCエラー数" v-text="String(pidValue(pid, 'errors', 2) ?? '—')" />
            </tr>
          </tbody>
        </table>
        <p v-else class="empty-state">ロス情報はありません</p>
      </div>
    </section>
    <p v-if="error" class="notice error" role="alert" v-text="error" />
  </section>
</template>

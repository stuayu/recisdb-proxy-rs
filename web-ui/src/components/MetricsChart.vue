<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { api, type JsonRecord } from '../api'

const props = defineProps<{ sessionId: string | number }>()
const data = ref<JsonRecord>({})
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
    data.value = await api(`/client/${props.sessionId}/metrics-history`)
    error.value = ''
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
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
          <polyline :points="bitrate"></polyline>
        </svg>
      </article>
      <article>
        <strong>信号レベル</strong>
        <svg :viewBox="viewBox" role="img" aria-label="信号レベル推移">
          <polyline :points="signal"></polyline>
        </svg>
      </article>
      <article>
        <strong>パケット損失</strong>
        <svg :viewBox="viewBox" role="img" aria-label="パケット損失推移">
          <polyline :points="loss"></polyline>
        </svg>
      </article>
    </div>
    <p v-if="error" class="notice error" role="alert" v-text="error"></p>
  </section>
</template>

<script setup lang="ts">
import { computed, ref } from 'vue'

const props = defineProps<{
  source: unknown
  unit: string
  label: string
  width: number
  height: number
}>()

type Sample = { x: number; y: number; value: number; ts: number | null }

function rawValues(source: unknown): { value: number; ts: number | null }[] {
  if (!Array.isArray(source)) return []
  return source
    .map((entry) => {
      if (Array.isArray(entry)) {
        const ts = Number(entry[0])
        return { value: Number(entry[1]), ts: Number.isFinite(ts) ? ts : null }
      }
      return { value: Number(entry), ts: null }
    })
    .filter((item) => Number.isFinite(item.value))
}

const padding = 6

const samples = computed<Sample[]>(() => {
  const items = rawValues(props.source)
  if (!items.length) return []
  const width = Math.max(100, props.width)
  const height = props.height
  const min = Math.min(...items.map((item) => item.value))
  const max = Math.max(...items.map((item) => item.value))
  const range = max - min || 1
  return items.map((item, index) => {
    const x = padding + (index / Math.max(1, items.length - 1)) * (width - padding * 2)
    const y = height - padding - ((item.value - min) / range) * (height - padding * 2)
    return { x, y, value: item.value, ts: item.ts }
  })
})

const points = computed(() =>
  samples.value.map((sample) => `${sample.x.toFixed(1)},${sample.y.toFixed(1)}`).join(' '),
)
const viewBox = computed(() => `0 0 ${Math.max(100, props.width)} ${props.height}`)

const svg = ref<SVGSVGElement | null>(null)
const hoverIndex = ref<number | null>(null)
const hovered = computed(() =>
  hoverIndex.value == null ? null : (samples.value[hoverIndex.value] ?? null),
)

function nearestIndex(clientX: number): number | null {
  if (!svg.value || !samples.value.length) return null
  const rect = svg.value.getBoundingClientRect()
  if (!rect.width) return null
  const viewWidth = Math.max(100, props.width)
  const localX = ((clientX - rect.left) / rect.width) * viewWidth
  let closest = 0
  let closestDist = Infinity
  samples.value.forEach((sample, index) => {
    const dist = Math.abs(sample.x - localX)
    if (dist < closestDist) {
      closestDist = dist
      closest = index
    }
  })
  return closest
}

function onMove(event: MouseEvent | TouchEvent) {
  const clientX = 'touches' in event ? event.touches[0]?.clientX : event.clientX
  if (clientX == null) return
  hoverIndex.value = nearestIndex(clientX)
}

function onLeave() {
  hoverIndex.value = null
}

function formatTime(ts: number | null): string {
  if (ts == null) return ''
  const date = new Date(ts)
  if (Number.isNaN(date.getTime())) return ''
  return date.toLocaleTimeString('ja-JP', { hour12: false })
}

const hoverLabel = computed(() => {
  const sample = hovered.value
  if (!sample) return ''
  const value = Number.isInteger(sample.value) ? String(sample.value) : sample.value.toFixed(2)
  const time = formatTime(sample.ts)
  return time ? `${time}  ${value} ${props.unit}` : `${value} ${props.unit}`
})

// Flip the tooltip box so it never spills past the chart edges.
const tooltipAnchor = computed(() =>
  (hovered.value?.x ?? 0) > Math.max(100, props.width) / 2 ? 'end' : 'start',
)
const tooltipY = computed(() => {
  const y = hovered.value?.y ?? 0
  return y > 24 ? y - 10 : y + 22
})
const tooltipBoxWidth = computed(() => Math.max(64, hoverLabel.value.length * 6.2 + 14))
</script>

<template>
  <svg
    ref="svg"
    :viewBox="viewBox"
    role="img"
    :aria-label="label"
    class="sparkline"
    @mousemove="onMove"
    @mouseleave="onLeave"
    @touchstart.passive="onMove"
    @touchmove.passive="onMove"
    @touchend="onLeave"
    @touchcancel="onLeave"
  >
    <polyline :points="points" />
    <g v-if="hovered">
      <line :x1="hovered.x" :x2="hovered.x" y1="0" :y2="height" class="hover-line" />
      <g :transform="`translate(${hovered.x}, ${tooltipY})`">
        <rect
          :x="tooltipAnchor === 'end' ? -tooltipBoxWidth : 0"
          y="-13"
          :width="tooltipBoxWidth"
          height="17"
          rx="3"
          class="hover-box"
        />
        <text
          :x="tooltipAnchor === 'end' ? -6 : 6"
          y="-1"
          :text-anchor="tooltipAnchor"
          class="hover-text"
          v-text="hoverLabel"
        />
      </g>
      <circle :cx="hovered.x" :cy="hovered.y" r="3.5" class="hover-dot" />
    </g>
  </svg>
</template>

<style scoped>
.sparkline {
  display: block;
  width: 100%;
  max-width: 100%;
  height: v-bind('`${height}px`');
  margin-top: 8px;
  cursor: crosshair;
  touch-action: pan-y;
}

.sparkline polyline {
  fill: none;
  stroke: var(--accent);
  stroke-width: 2;
  vector-effect: non-scaling-stroke;
}

.hover-line {
  stroke: var(--muted);
  stroke-width: 1;
  stroke-dasharray: 2 2;
  vector-effect: non-scaling-stroke;
}

.hover-dot {
  fill: var(--accent);
  stroke: var(--surface);
  stroke-width: 1.5;
  vector-effect: non-scaling-stroke;
}

.hover-box {
  fill: var(--surface);
  stroke: var(--border);
  stroke-width: 1;
  vector-effect: non-scaling-stroke;
}

.hover-text {
  fill: var(--text);
  font-size: 9px;
  font-weight: 600;
}
</style>

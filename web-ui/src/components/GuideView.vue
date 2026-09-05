<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, shallowRef, watch } from 'vue'
import { api, unwrapArray, type JsonRecord } from '../api'
import PreviewPlayer from './PreviewPlayer.vue'

const GRID_START_HOUR = 6
const GRID_HOURS = 24
const TOTAL_MINUTES = GRID_HOURS * 60
const MAX_PROGRAM_DURATION_SECS = 24 * 60 * 60
const HEADER_HEIGHT = 52
const NARROW_MEDIA_QUERY = '(max-width: 700px)'

/*
 * 表示密度はデバイスで変える (KonomiTV の TimeTableUtils.CHANNEL_WIDTH / HOUR_HEIGHT と同じ考え方)。
 * 本番は放送局が 800 前後あり、1 列 220px のままだと横幅が 5 万px を超えて
 * スマホでは 2 列も入らない。狭幅では列を詰め、1 分あたりの高さも下げる。
 */
const PX_PER_MIN_DESKTOP = 2
const PX_PER_MIN_NARROW = 1.6
const COLUMN_WIDTH_DESKTOP = 220
const COLUMN_WIDTH_NARROW = 120
const AXIS_WIDTH_DESKTOP = 64
const AXIS_WIDTH_NARROW = 44

/*
 * 可視判定のバッファ。KonomiTV は デスクトップ 2 時間 / スマホ 3 時間で、
 * 指スクロールで一気に動く狭幅ほど厚くしている (VISIBLE_BUFFER_HOURS_*)。
 * VISIBLE_RANGE_UPDATE_RATIO も KonomiTV と同じで、バッファの端に近づいたときだけ
 * 範囲を引き直す (スクロールのたびに再計算しない)。
 */
const VISIBLE_BUFFER_MINUTES_DESKTOP = 120
const VISIBLE_BUFFER_MINUTES_NARROW = 180
const VISIBLE_RANGE_UPDATE_RATIO = 0.5
const COLUMN_BUFFER_DESKTOP = 2
const COLUMN_BUFFER_NARROW = 3

const GENRE_LABELS: Record<number, string> = {
  0: 'ニュース/報道',
  1: 'スポーツ',
  2: '情報/ワイドショー',
  3: 'ドラマ',
  4: '音楽',
  5: 'バラエティ',
  6: '映画',
  7: 'アニメ/特撮',
  8: 'ドキュメンタリー/教養',
  9: '劇場/公演',
  10: '趣味/教育',
  11: '福祉',
  14: '拡張',
  15: 'その他',
}
const GENRE_COLORS: Record<number, string> = {
  0: '#3b82f6',
  1: '#22c55e',
  2: '#f59e0b',
  3: '#ec4899',
  4: '#a855f7',
  5: '#f97316',
  6: '#ef4444',
  7: '#06b6d4',
  8: '#14b8a6',
  9: '#eab308',
  10: '#84cc16',
  11: '#64748b',
  14: '#6366f1',
  15: '#9ca3af',
}
type BandCategory = '地上' | 'BS' | 'CS' | 'その他'
type Service = {
  key: string
  nid: number
  sid: number
  tsid: number
  name: string
  band: BandCategory
  remoteControlKey: number | null
  region: string | null
}
type Program = {
  id: number
  nid: number
  sid: number
  event_id: number
  start_at: number
  duration_secs: number
  name: string
  description: string
  extended: string
  genre: number | null
}
/** 事前に位置とスタイルまで計算した番組セル。スクロール中は作り直さない。 */
type RenderItem = {
  program: Program
  top: number
  bottom: number
  style: Record<string, string>
}
/** 1 列 (= 同一 nid:tsid の多重化。メイン + 併置するサブチャンネル)。 */
type GuideColumn = {
  key: string
  name: string
  subLabel: string
  band: BandCategory
  nid: number
  sid: number
  items: RenderItem[]
}
const BAND_ORDER: Record<BandCategory, number> = {
  地上: 0,
  BS: 1,
  CS: 2,
  その他: 3,
}

function bandCategory(value: unknown, nid: number): BandCategory {
  if (value !== null && value !== undefined && Number.isFinite(Number(value))) {
    if (Number(value) === 0 || Number(value) === 5) return '地上'
    if (Number(value) === 1 || Number(value) === 3) return 'BS'
    if (Number(value) === 2 || Number(value) === 6) return 'CS'
    return 'その他'
  }
  if (nid === 4) return 'BS'
  if (nid === 6 || nid === 7) return 'CS'
  if (nid >= 0x7880 && nid <= 0x7fef) return '地上'
  return 'その他'
}
function isGuideServiceType(value: unknown): boolean {
  return value === null || value === undefined || Number(value) === 1 || Number(value) === 0xad
}
function genreLevel(genre: number | null): number | null {
  return genre === null ? null : (genre >> 4) & 0xf
}
function genreLabel(genre: number | null): string {
  const level = genreLevel(genre)
  return level === null ? '—' : (GENRE_LABELS[level] ?? `不明(0x${level.toString(16)})`)
}
function genreColor(genre: number | null): string {
  const level = genreLevel(genre)
  return level === null ? 'transparent' : (GENRE_COLORS[level] ?? 'var(--muted)')
}
function fmtDateInput(date: Date): string {
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`
}
function fmtTime(epoch: number): string {
  const date = new Date(epoch * 1000)
  return `${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}`
}

/*
 * 本番の /programs は 2 万件を超える。ref だと配列の要素まで再帰的に Proxy 化され、
 * 分類ループが 2 万個 × 各プロパティぶんの依存を張って一気に重くなる。
 * 差し替えしか起きないので shallowRef で持つ (KonomiTV の TimeTableStore も
 * channels_data を shallowRef にしている)。
 */
const rawChannels = shallowRef<JsonRecord[]>([])
const rawPrograms = shallowRef<JsonRecord[]>([])
const error = ref('')
const loading = ref(false)
const selectedDate = ref(fmtDateInput(new Date()))
const bandFilter = ref<'すべて' | BandCategory>('すべて')
const regionFilter = ref('すべて')
const serviceQuery = ref('')
const now = ref(Date.now())
const detail = ref<Program | null>(null)
const previewProgram = ref<Program | null>(null)
const scrollArea = ref<HTMLElement | null>(null)
const isNarrow = ref(false)
const viewportHeight = ref(0)
const visibleRangeTop = ref(0)
const visibleRangeBottom = ref(0)
const visibleColumnStart = ref(0)
const visibleColumnEnd = ref(0)
let scrollAnimationId: number | null = null
let pendingScrollTop = 0
let pendingScrollLeft = 0
let clockTimer = 0
let narrowMedia: MediaQueryList | null = null

const pxPerMin = computed(() => (isNarrow.value ? PX_PER_MIN_NARROW : PX_PER_MIN_DESKTOP))
const columnWidth = computed(() => (isNarrow.value ? COLUMN_WIDTH_NARROW : COLUMN_WIDTH_DESKTOP))
const axisWidth = computed(() => (isNarrow.value ? AXIS_WIDTH_NARROW : AXIS_WIDTH_DESKTOP))
const totalHeight = computed(() => TOTAL_MINUTES * pxPerMin.value)
const visibleBufferPx = computed(
  () =>
    (isNarrow.value ? VISIBLE_BUFFER_MINUTES_NARROW : VISIBLE_BUFFER_MINUTES_DESKTOP) *
    pxPerMin.value,
)
const columnBuffer = computed(() => (isNarrow.value ? COLUMN_BUFFER_NARROW : COLUMN_BUFFER_DESKTOP))

const gridStart = computed(() => {
  const [year, month, day] = selectedDate.value.split('-').map(Number)
  return new Date(year, month - 1, day, GRID_START_HOUR)
})
const gridBounds = computed(() => {
  const since = Math.floor(gridStart.value.getTime() / 1000)
  return { since, until: since + TOTAL_MINUTES * 60 }
})
const isToday = computed(() => selectedDate.value === fmtDateInput(new Date()))
const nowOffset = computed(() => ((now.value / 1000 - gridBounds.value.since) / 60) * pxPerMin.value)
const showNowLine = computed(
  () => isToday.value && nowOffset.value >= 0 && nowOffset.value <= totalHeight.value,
)
const hourMarks = computed(() =>
  Array.from({ length: GRID_HOURS }, (_, index) => ({
    offset: index * 60 * pxPerMin.value,
    label: `${String((GRID_START_HOUR + index) % 24).padStart(2, '0')}:00`,
  })),
)

/** 縦方向の可視範囲を引き直す。 */
function updateVisibleRange(anchor: number): void {
  const buffer = visibleBufferPx.value
  visibleRangeTop.value = Math.max(0, anchor - buffer)
  visibleRangeBottom.value = Math.min(totalHeight.value, anchor + viewportHeight.value + buffer)
}
/**
 * バッファの端に近づいたときだけ可視範囲を引き直す。
 * (KonomiTV TimeTableGrid.updateVisibleRangeIfNeeded と同じヒステリシス)
 */
function updateVisibleRangeIfNeeded(anchor: number): void {
  if (viewportHeight.value <= 0 || visibleRangeBottom.value === 0) {
    updateVisibleRange(anchor)
    return
  }
  const threshold = visibleBufferPx.value * VISIBLE_RANGE_UPDATE_RATIO
  const nearTop = visibleRangeTop.value > 0 && anchor < visibleRangeTop.value + threshold
  const nearBottom =
    visibleRangeBottom.value < totalHeight.value &&
    anchor + viewportHeight.value > visibleRangeBottom.value - threshold
  if (nearTop || nearBottom) updateVisibleRange(anchor)
}
/**
 * 横方向の可視範囲 (列の添字) を引き直す。
 * 本番は列が 200 を超え、画面には数列しか映らない。列そのものを DOM から外す。
 */
function updateVisibleColumns(scrollLeft: number, clientWidth: number): void {
  const width = columnWidth.value
  const total = columns.value.length
  const buffer = columnBuffer.value
  const first = Math.max(0, Math.floor((scrollLeft - axisWidth.value) / width) - buffer)
  const last = Math.min(
    total,
    Math.ceil((scrollLeft + clientWidth - axisWidth.value) / width) + buffer,
  )
  if (visibleColumnStart.value !== first) visibleColumnStart.value = first
  if (visibleColumnEnd.value !== Math.max(first, last)) {
    visibleColumnEnd.value = Math.max(first, last)
  }
}
function applyScrollUpdate(): void {
  const element = scrollArea.value
  if (element === null) return
  const nextViewportHeight = element.clientHeight
  if (viewportHeight.value !== nextViewportHeight) viewportHeight.value = nextViewportHeight
  updateVisibleRangeIfNeeded(pendingScrollTop - HEADER_HEIGHT)
  updateVisibleColumns(pendingScrollLeft, element.clientWidth)
}
function scheduleScrollUpdate(): void {
  if (scrollAnimationId !== null) return
  scrollAnimationId = requestAnimationFrame(() => {
    scrollAnimationId = null
    applyScrollUpdate()
  })
}
function onScroll(): void {
  const element = scrollArea.value
  pendingScrollTop = element?.scrollTop ?? 0
  pendingScrollLeft = element?.scrollLeft ?? 0
  scheduleScrollUpdate()
}
/** 可視範囲を無条件で作り直す (初期表示・リサイズ・データ差し替え時)。 */
function resizeGrid(): void {
  const element = scrollArea.value
  isNarrow.value = narrowMedia?.matches ?? window.innerWidth <= 700
  viewportHeight.value = element?.clientHeight ?? 0
  pendingScrollTop = element?.scrollTop ?? 0
  pendingScrollLeft = element?.scrollLeft ?? 0
  updateVisibleRange(pendingScrollTop - HEADER_HEIGHT)
  updateVisibleColumns(pendingScrollLeft, element?.clientWidth ?? 0)
}

async function loadChannels() {
  try {
    rawChannels.value = unwrapArray(await api('/channels'), ['channels'])
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}
async function loadPrograms() {
  loading.value = true
  try {
    const { since, until } = gridBounds.value
    rawPrograms.value = unwrapArray(await api(`/programs?since=${since}&until=${until}`), [
      'programs',
    ])
    error.value = ''
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    loading.value = false
  }
}
async function refresh() {
  await Promise.all([loadChannels(), loadPrograms()])
  await nextTick()
  resizeGrid()
}

const services = computed<Service[]>(() => {
  const seen = new Map<string, Service>()
  for (const row of rawChannels.value) {
    const nid = Number(row.nid),
      sid = Number(row.sid),
      tsid = Number(row.tsid)
    if (![nid, sid, tsid].every(Number.isFinite) || !isGuideServiceType(row.service_type)) continue
    const key = `${nid}:${sid}`
    if (seen.has(key)) continue
    const remote = row.remote_control_key == null ? null : Number(row.remote_control_key)
    seen.set(key, {
      key,
      nid,
      sid,
      tsid,
      name: String(row.channel_name ?? `${nid}-${sid}`),
      band: bandCategory(row.band_type, nid),
      remoteControlKey: Number.isFinite(remote) ? remote : null,
      region: row.terrestrial_region == null ? null : String(row.terrestrial_region),
    })
  }
  return [...seen.values()].sort(
    (a, b) =>
      BAND_ORDER[a.band] - BAND_ORDER[b.band] ||
      a.nid - b.nid ||
      (a.remoteControlKey ?? 999) - (b.remoteControlKey ?? 999) ||
      a.sid - b.sid,
  )
})
const regionOptions = computed(() => [
  ...new Set(
    services.value
      .filter((service) => service.band === '地上' && service.region)
      .map((service) => service.region as string),
  ),
])
watch(bandFilter, (value) => {
  if (value !== '地上' && value !== 'すべて') regionFilter.value = 'すべて'
})
watch(regionFilter, (value) => {
  if (value !== 'すべて' && bandFilter.value !== '地上') bandFilter.value = '地上'
})
const filteredServices = computed(() => {
  const query = serviceQuery.value.trim().toLowerCase()
  return services.value.filter(
    (service) =>
      (bandFilter.value === 'すべて' || service.band === bandFilter.value) &&
      (regionFilter.value === 'すべて' || service.region === regionFilter.value) &&
      (!query ||
        service.name.toLowerCase().includes(query) ||
        String(service.nid).includes(query) ||
        String(service.sid).includes(query)),
  )
})
/**
 * サービスごとの番組。rawPrograms が差し替わったときだけ作り直す。
 * (KonomiTV が親ストアで局別に分けた配列を配るのと同じ役割)
 */
const programsByService = computed(() => {
  const result = new Map<string, Program[]>()
  for (const row of rawPrograms.value) {
    const nid = Number(row.nid),
      sid = Number(row.sid),
      start = Number(row.start_at),
      duration = Number(row.duration_secs)
    if (
      ![nid, sid, start, duration].every(Number.isFinite) ||
      duration < 0 ||
      duration > MAX_PROGRAM_DURATION_SECS
    )
      continue
    const program: Program = {
      id: Number(row.id),
      nid,
      sid,
      event_id: Number(row.event_id),
      start_at: start,
      duration_secs: duration,
      name: row.name == null ? '' : String(row.name),
      description: row.description == null ? '' : String(row.description),
      extended: row.extended == null ? '' : String(row.extended),
      genre: row.genre == null ? null : Number(row.genre),
    }
    const list = result.get(`${nid}:${sid}`)
    if (list) list.push(program)
    else result.set(`${nid}:${sid}`, [program])
  }
  for (const list of result.values()) list.sort((a, b) => a.start_at - b.start_at)
  return result
})
function programSlotKey(program: Program): string | null {
  if (!program.name.trim()) return null
  return `${program.start_at}:${program.duration_secs}:${program.name}`
}
function sameSlotSets(mainPrograms: Program[], subPrograms: Program[]): boolean {
  if (mainPrograms.length !== subPrograms.length || mainPrograms.length === 0) return false
  const mainKeys = mainPrograms.map(programSlotKey)
  const subKeys = subPrograms.map(programSlotKey)
  if (mainKeys.some((key) => key === null) || subKeys.some((key) => key === null)) return false
  const subKeySet = new Set(subKeys)
  return mainKeys.every((key) => subKeySet.has(key))
}
/** 重なり判定用に、番組の占有区間を重複のない昇順区間へ畳む。 */
function mergeSpans(programs: Program[]): Array<[number, number]> {
  const spans = programs
    .map((program): [number, number] => [
      program.start_at,
      program.start_at + program.duration_secs,
    ])
    .sort((a, b) => a[0] - b[0])
  const merged: Array<[number, number]> = []
  for (const span of spans) {
    const last = merged[merged.length - 1]
    if (last !== undefined && span[0] <= last[1]) {
      if (span[1] > last[1]) last[1] = span[1]
    } else merged.push([span[0], span[1]])
  }
  return merged
}
/** 畳んだ区間に対する二分探索。総当たりだと列あたり O(番組数^2) になる。 */
function overlapsSpans(spans: Array<[number, number]>, start: number, end: number): boolean {
  let low = 0
  let high = spans.length
  while (low < high) {
    const mid = (low + high) >> 1
    if (spans[mid][1] <= start) low = mid + 1
    else high = mid
  }
  return low < spans.length && spans[low][0] < end
}
/**
 * 列と、その中の番組セルの位置・スタイルまでを一度に組み立てる。
 * filteredServices / programsByService / 表示密度 が変わったときだけ走り、
 * スクロール中は一切再計算しない。
 */
const columns = computed<GuideColumn[]>(() => {
  const byService = programsByService.value
  const { since, until } = gridBounds.value
  const perMin = pxPerMin.value
  const height = totalHeight.value
  const grouped = new Map<string, Service[]>()
  for (const service of filteredServices.value) {
    const key = `${service.nid}:${service.tsid}`
    const list = grouped.get(key)
    if (list) list.push(service)
    else grouped.set(key, [service])
  }
  const result: GuideColumn[] = []
  for (const [key, list] of grouped) {
    const sorted = list.length > 1 ? [...list].sort((a, b) => a.sid - b.sid) : list
    const main = sorted[0]
    const mainPrograms = byService.get(main.key) ?? []
    // メインと EPG が同一のサブチャンネルは列に出さない (メインが全幅で残る)。
    const subs = sorted
      .slice(1)
      .filter((service) => {
        const subPrograms = byService.get(service.key) ?? []
        return subPrograms.length > 0 && !sameSlotSets(mainPrograms, subPrograms)
      })
      .map((service) => ({ service, programs: byService.get(service.key) ?? [] }))
    const subPrograms = subs.flatMap((entry) => entry.programs)
    const mainSpans = subPrograms.length > 0 ? mergeSpans(mainPrograms) : []
    const subSpans = subPrograms.length > 0 ? mergeSpans(subPrograms) : []
    const items: RenderItem[] = []
    const push = (program: Program, isSub: boolean, split: boolean): void => {
      const end = program.start_at + program.duration_secs
      if (end <= since || program.start_at >= until) return
      const top = Math.max(0, (program.start_at - since) / 60) * perMin
      const bottom = Math.min(TOTAL_MINUTES, (end - since) / 60) * perMin
      const color = genreColor(program.genre)
      items.push({
        program,
        top,
        bottom: Math.min(height, Math.max(bottom, top + 2)),
        style: {
          top: `${top}px`,
          height: `${Math.max(bottom - top, 2)}px`,
          left: isSub && split ? '50%' : '0',
          width: split ? '50%' : '100%',
          borderLeftColor: color,
          background: `color-mix(in srgb, ${color} 12%, var(--surface))`,
        },
      })
    }
    for (const program of mainPrograms) {
      const end = program.start_at + program.duration_secs
      push(program, false, subSpans.length > 0 && overlapsSpans(subSpans, program.start_at, end))
    }
    for (const entry of subs) {
      for (const program of entry.programs) {
        const end = program.start_at + program.duration_secs
        push(program, true, overlapsSpans(mainSpans, program.start_at, end))
      }
    }
    // 上から順に並べておくと、可視判定を先頭から走らせて途中で打ち切れる。
    items.sort((a, b) => a.top - b.top)
    result.push({
      key,
      name: main.name,
      subLabel: subs.map((entry) => entry.service.name).join(' / '),
      band: main.band,
      nid: main.nid,
      sid: main.sid,
      items,
    })
  }
  // 並びは従来どおり 地上 → BS → CS → その他、その中は nid / sid 昇順。
  return result.sort(
    (a, b) => BAND_ORDER[a.band] - BAND_ORDER[b.band] || a.nid - b.nid || a.sid - b.sid,
  )
})
/** 画面に入っている列だけを切り出す。 */
const visibleColumns = computed(() => {
  const all = columns.value
  const first = Math.min(visibleColumnStart.value, Math.max(0, all.length - 1))
  const last = Math.min(visibleColumnEnd.value, all.length)
  return all.slice(first, last).map((column, offset) => ({ column, index: first + offset }))
})
/** 列の中で可視範囲に入る番組セル。items は top 昇順なので途中で打ち切れる。 */
function visibleItems(column: GuideColumn): RenderItem[] {
  const top = visibleRangeTop.value
  const bottom = visibleRangeBottom.value
  const result: RenderItem[] = []
  for (const item of column.items) {
    if (item.top > bottom) break
    if (item.bottom >= top) result.push(item)
  }
  return result
}
function shiftDate(days: number) {
  const [y, m, d] = selectedDate.value.split('-').map(Number)
  const date = new Date(y, m - 1, d)
  date.setDate(date.getDate() + days)
  selectedDate.value = fmtDateInput(date)
}
function goToday() {
  selectedDate.value = fmtDateInput(new Date())
}
function openDetail(program: Program) {
  detail.value = program
}
function closeDetail() {
  detail.value = null
}
function openPreview(program: Program) {
  detail.value = null
  previewProgram.value = program
}
function closePreview() {
  previewProgram.value = null
}
function onKeydown(event: KeyboardEvent) {
  if (event.key !== 'Escape') return
  if (previewProgram.value) closePreview()
  else closeDetail()
}
watch(selectedDate, () => void loadPrograms())
// 絞り込みや表示密度で列数が変わったら、可視範囲を取り直す。
watch([columns, pxPerMin], () => void nextTick().then(resizeGrid))
onMounted(() => {
  narrowMedia = window.matchMedia(NARROW_MEDIA_QUERY)
  narrowMedia.addEventListener('change', resizeGrid)
  void refresh()
  resizeGrid()
  clockTimer = window.setInterval(() => {
    now.value = Date.now()
  }, 30000)
  window.addEventListener('resize', resizeGrid)
  window.addEventListener('keydown', onKeydown)
})
onUnmounted(() => {
  window.clearInterval(clockTimer)
  narrowMedia?.removeEventListener('change', resizeGrid)
  window.removeEventListener('resize', resizeGrid)
  window.removeEventListener('keydown', onKeydown)
  if (scrollAnimationId !== null) cancelAnimationFrame(scrollAnimationId)
})
</script>

<template>
  <section class="view guide-view">
    <div class="view-heading">
      <div>
        <h2>番組表</h2>
        <p>視聴中チューナーが収集したEPGデータを表示します（歯抜けは正常です）</p>
      </div>
      <button class="button secondary" @click="refresh" v-text="loading ? '更新中…' : '更新'" />
    </div>
    <div class="guide-toolbar">
      <div class="guide-date-nav">
        <button class="button small secondary" aria-label="前日" @click="shiftDate(-1)">◀</button
        ><button class="button small secondary" @click="goToday">今日</button
        ><button class="button small secondary" aria-label="翌日" @click="shiftDate(1)">▶</button
        ><input v-model="selectedDate" type="date" aria-label="日付を選択" />
      </div>
      <label class="field guide-band-filter"
        ><span>放送種別</span
        ><select v-model="bandFilter">
          <option value="すべて">すべて</option>
          <option value="地上">地上波</option>
          <option value="BS">BS</option>
          <option value="CS">CS</option>
        </select></label
      ><label class="field guide-region-filter"
        ><span>地域（地上）</span
        ><select v-model="regionFilter" :disabled="!regionOptions.length">
          <option value="すべて">すべての地域</option>
          <option
            v-for="region in regionOptions"
            :key="region"
            :value="region"
            v-text="region"
          /></select></label
      ><label class="search guide-service-search"
        ><span>サービス絞り込み</span
        ><input v-model="serviceQuery" type="search" placeholder="チャンネル名、NID、SID"
      /></label>
    </div>
    <p v-if="error" class="notice error" role="alert" v-text="error" />
    <p v-if="!rawPrograms.length && !loading" class="empty-state">
      番組情報がありません。番組情報は視聴中のチャンネルから自動収集されます。
    </p>
    <div v-else ref="scrollArea" class="guide-scroll" @scroll.passive="onScroll">
      <div
        class="guide-grid"
        :style="{
          width: `${axisWidth + columns.length * columnWidth}px`,
          height: `${totalHeight + HEADER_HEIGHT}px`,
          '--guide-col-w': `${columnWidth}px`,
          '--guide-hour-h': `${60 * pxPerMin}px`,
        }"
      >
        <div class="guide-header-row" :style="{ height: `${HEADER_HEIGHT}px` }">
          <div class="guide-corner" :style="{ width: `${axisWidth}px` }" />
          <div
            v-for="entry in visibleColumns"
            :key="`h-${entry.column.key}`"
            class="guide-header-cell"
            :style="{ left: `${axisWidth + entry.index * columnWidth}px`, width: `${columnWidth}px` }"
          >
            <span v-text="entry.column.name" /><small
              v-if="entry.column.subLabel"
              v-text="entry.column.subLabel"
            />
          </div>
        </div>
        <div class="guide-body" :style="{ height: `${totalHeight}px` }">
          <div class="guide-channel-background" :style="{ left: `${axisWidth}px` }" />
          <div class="guide-timeaxis" :style="{ width: `${axisWidth}px`, height: `${totalHeight}px` }">
            <div
              v-for="mark in hourMarks"
              :key="mark.label"
              class="guide-hour-label"
              :style="{ top: `${mark.offset}px` }"
              v-text="mark.label"
            />
          </div>
          <div
            v-if="showNowLine"
            class="guide-now-line"
            :style="{ top: `${nowOffset}px`, left: `${axisWidth}px` }"
          />
          <div
            v-for="entry in visibleColumns"
            :key="`c-${entry.column.key}`"
            class="guide-col"
            :style="{
              left: `${axisWidth + entry.index * columnWidth}px`,
              width: `${columnWidth}px`,
              height: `${totalHeight}px`,
            }"
          >
            <button
              v-for="item in visibleItems(entry.column)"
              :key="item.program.id"
              type="button"
              class="guide-cell"
              :aria-label="item.program.name || '番組名なし'"
              :style="item.style"
              @click="openDetail(item.program)"
            >
              <strong v-if="item.program.name" v-text="item.program.name" /><span
                v-else
                class="guide-untitled"
                >番組名なし</span
              ><span class="guide-cell-time" v-text="fmtTime(item.program.start_at)" />
            </button>
          </div>
        </div>
      </div>
      <p v-if="!columns.length" class="empty-state">条件に一致するサービスがありません</p>
    </div>
    <div v-if="detail" class="dialog-backdrop" @click.self="closeDetail">
      <section
        class="dialog guide-detail-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="guide-detail-title"
      >
        <h2 id="guide-detail-title" v-text="detail.name || '番組情報'" />
        <p class="guide-detail-time">
          <span v-text="fmtTime(detail.start_at)" />〜<span
            v-text="fmtTime(detail.start_at + detail.duration_secs)"
          />
        </p>
        <p class="guide-detail-genre">
          ジャンル:
          <span
            class="genre-badge"
            :style="{ borderColor: genreColor(detail.genre) }"
            v-text="genreLabel(detail.genre)"
          />
        </p>
        <p v-if="detail.description" class="preserve-lines" v-text="detail.description" />
        <p
          v-if="detail.extended"
          class="preserve-lines guide-detail-extended"
          v-text="detail.extended"
        />
        <div class="actions">
          <button class="button" @click="openPreview(detail)">▶ プレビュー</button
          ><button class="button secondary" @click="closeDetail">閉じる</button>
        </div>
      </section>
    </div>
    <div v-if="previewProgram" class="dialog-backdrop" @click.self="closePreview">
      <section
        class="dialog preview-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="guide-preview-player-title"
      >
        <div class="view-heading">
          <div>
            <h2 id="guide-preview-player-title">ブラウザプレビュー</h2>
            <p
              class="muted"
              v-text="`${previewProgram.name || '番組名なし'}（SID ${previewProgram.sid}）`"
            />
          </div>
          <button class="button secondary" @click="closePreview">閉じる</button>
        </div>
        <PreviewPlayer
          :key="`${previewProgram.nid}-${previewProgram.sid}`"
          :initial-sid="previewProgram.sid"
          :initial-nid="previewProgram.nid"
        />
      </section>
    </div>
  </section>
</template>

<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { api, unwrapArray, type JsonRecord } from '../api'
import PreviewPlayer from './PreviewPlayer.vue'

const PX_PER_MIN = 2
const GRID_START_HOUR = 6
const TOTAL_MINUTES = 24 * 60
const TOTAL_HEIGHT = TOTAL_MINUTES * PX_PER_MIN
const MAX_PROGRAM_DURATION_SECS = 24 * 60 * 60
const VISIBLE_BUFFER_MINUTES = 180
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
type ServiceGroup = {
  key: string
  main: Service
  subs: Service[]
  band: BandCategory
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

const rawChannels = ref<JsonRecord[]>([])
const rawPrograms = ref<JsonRecord[]>([])
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
const viewportHeight = ref(0)
const visibleRangeTop = ref(0)
const visibleRangeBottom = ref(TOTAL_HEIGHT)
let scrollAnimationId: number | null = null
let pendingScrollTop = 0
let clockTimer = 0
const gridStart = computed(() => {
  const [year, month, day] = selectedDate.value.split('-').map(Number)
  return new Date(year, month - 1, day, GRID_START_HOUR)
})
const gridBounds = computed(() => {
  const since = Math.floor(gridStart.value.getTime() / 1000)
  return { since, until: since + TOTAL_MINUTES * 60 }
})
const isToday = computed(() => selectedDate.value === fmtDateInput(new Date()))
const nowOffset = computed(() => ((now.value / 1000 - gridBounds.value.since) / 60) * PX_PER_MIN)
const showNowLine = computed(
  () => isToday.value && nowOffset.value >= 0 && nowOffset.value <= TOTAL_HEIGHT,
)
const hourMarks = computed(() =>
  Array.from({ length: 24 }, (_, index) => ({
    offset: index * 60 * PX_PER_MIN,
    label: `${String((GRID_START_HOUR + index) % 24).padStart(2, '0')}:00`,
  })),
)
function updateVisibleRange(top: number) {
  const buffer = VISIBLE_BUFFER_MINUTES * PX_PER_MIN
  visibleRangeTop.value = Math.max(0, top - buffer)
  visibleRangeBottom.value = Math.min(TOTAL_HEIGHT, top + viewportHeight.value + buffer)
}
function scheduleScrollUpdate() {
  if (scrollAnimationId !== null) return
  scrollAnimationId = requestAnimationFrame(() => {
    scrollAnimationId = null
    updateVisibleRange(pendingScrollTop)
  })
}
function onScroll() {
  pendingScrollTop = scrollArea.value?.scrollTop ?? 0
  scheduleScrollUpdate()
}
function resizeGrid() {
  viewportHeight.value = scrollArea.value?.clientHeight ?? 0
  updateVisibleRange(scrollArea.value?.scrollTop ?? 0)
}
function isProgramVisible(program: Program): boolean {
  const start = ((program.start_at - gridBounds.value.since) / 60) * PX_PER_MIN
  const end = start + (program.duration_secs / 60) * PX_PER_MIN
  return end >= visibleRangeTop.value && start <= visibleRangeBottom.value
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
function programsFor(service: Service): Program[] {
  return programsByService.value.get(service.key) ?? []
}
const groups = computed<ServiceGroup[]>(() => {
  const map = new Map<string, Service[]>()
  for (const service of filteredServices.value) {
    const key = `${service.nid}:${service.tsid}`
    const list = map.get(key)
    if (list) list.push(service)
    else map.set(key, [service])
  }
  return [...map]
    .map(([key, list]) => {
      const sorted = [...list].sort((a, b) => a.sid - b.sid)
      const main = sorted[0]
      const mainPrograms = programsFor(main)
      const subs = sorted.slice(1).filter((service) => {
        const subPrograms = programsFor(service)
        if (subPrograms.length === 0) return false
        return !sameSlotSets(mainPrograms, subPrograms)
      })
      return { key, main, subs, band: main.band }
    })
    .sort(
      (a, b) =>
        BAND_ORDER[a.band] - BAND_ORDER[b.band] ||
        a.main.nid - b.main.nid ||
        a.main.sid - b.main.sid,
    )
})
function overlaps(program: Program, others: Program[]): boolean {
  const end = program.start_at + program.duration_secs
  return others.some(
    (other) => other.start_at < end && other.start_at + other.duration_secs > program.start_at,
  )
}
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
function programsForRender(group: ServiceGroup) {
  const subPrograms = group.subs.flatMap(programsFor)
  return [
    ...programsFor(group.main).map((program) => ({
      program,
      isSub: false,
      split: subPrograms.length > 0 && overlaps(program, subPrograms),
    })),
    ...group.subs.flatMap((service) =>
      programsFor(service).map((program) => ({
        program,
        isSub: true,
        split: overlaps(program, programsFor(group.main)),
      })),
    ),
  ].filter((item) => isProgramVisible(item.program))
}
function cellStyle(item: { program: Program; isSub: boolean; split: boolean }) {
  const start = Math.max(0, (item.program.start_at - gridBounds.value.since) / 60) * PX_PER_MIN
  const end =
    Math.min(
      TOTAL_MINUTES,
      (item.program.start_at + item.program.duration_secs - gridBounds.value.since) / 60,
    ) * PX_PER_MIN
  return {
    top: `${start}px`,
    height: `${Math.max(end - start, 2)}px`,
    left: item.isSub && item.split ? '50%' : '0',
    width: item.split ? '50%' : '100%',
    borderLeftColor: genreColor(item.program.genre),
    background: `color-mix(in srgb, ${genreColor(item.program.genre)} 12%, var(--surface))`,
  }
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
onMounted(() => {
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
    <div v-else ref="scrollArea" class="guide-scroll" @scroll="onScroll">
      <div
        class="guide-grid"
        :style="{
          width: `${64 + groups.length * 220}px`,
          height: `${TOTAL_HEIGHT + 52}px`,
          '--guide-columns': groups.length,
        }"
      >
        <div class="guide-corner" />
        <div v-for="group in groups" :key="`h-${group.key}`" class="guide-header-cell">
          <span v-text="group.main.name" /><small
            v-if="group.subs.length"
            v-text="group.subs.map((service) => service.name).join(' / ')"
          />
        </div>
        <div class="guide-timeaxis" :style="{ height: `${TOTAL_HEIGHT}px` }">
          <div
            v-for="mark in hourMarks"
            :key="mark.label"
            class="guide-hour-label"
            :style="{ top: `${mark.offset}px` }"
            v-text="mark.label"
          />
        </div>
        <div
          v-for="group in groups"
          :key="`c-${group.key}`"
          class="guide-col"
          :style="{ height: `${TOTAL_HEIGHT}px` }"
        >
          <div class="guide-channel-background" :style="{ height: `${TOTAL_HEIGHT}px` }" />
          <div v-if="showNowLine" class="guide-now-line" :style="{ top: `${nowOffset}px` }" />
          <button
            v-for="item in programsForRender(group)"
            :key="item.program.id"
            type="button"
            class="guide-cell"
            :aria-label="item.program.name || '番組名なし'"
            :style="cellStyle(item)"
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
      <p v-if="!groups.length" class="empty-state">条件に一致するサービスがありません</p>
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

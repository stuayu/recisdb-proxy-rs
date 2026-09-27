<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, shallowRef, watch } from 'vue'
import { api, unwrapArray, type JsonRecord } from '../api'
import {
  buildProgramIndex,
  epgStatusPresentation,
  mergeProgramEvent,
  normalizeProgramRow,
  programEventKey,
  useEpgEvents,
  type EpgProgramEvent,
  type EpgScanState,
} from '../composables/useEpgEvents'
import { broadcastDateInput } from '../composables/useGuideDate'
import { calculateGuideProgramWindow } from '../composables/useGuideProgramWindow'
import { guideChannelNumber, guideServiceGroupKey, isPlaceholderProgramName, isRealGuideService, mainGuideServices } from '../composables/useGuideServices'
import PreviewPlayer from './PreviewPlayer.vue'

const GRID_START_HOUR = 6
const GRID_HOURS = 24
const TOTAL_MINUTES = GRID_HOURS * 60
const MAX_PROGRAM_DURATION_SECS = 24 * 60 * 60
const NARROW_MEDIA_QUERY = '(max-width: 700px)'

/*
 * 表示密度はデバイスで変える (KonomiTV の TimeTableUtils.CHANNEL_WIDTH / HOUR_HEIGHT と同じ考え方)。
 * 本番は放送局が 800 前後あり、1 列 220px のままだと横幅が 5 万px を超えて
 * スマホでは 2 列も入らない。狭幅では列を詰め、1 分あたりの高さも下げる。
 */
const AXIS_WIDTH_DESKTOP = 44
const AXIS_WIDTH_NARROW = 30
const DEFAULT_CHANNEL_COUNT = 7
const DEFAULT_CHANNEL_COUNT_NARROW = 5
const DEFAULT_HOURS = 4
const CHANNEL_COUNT_OPTIONS = [5, 7, 9] as const
const HOURS_OPTIONS = [3, 4, 6] as const
const DENSITY_STORAGE_KEY = 'guide:density'
const DETAIL_LOAD_DEBOUNCE_MS = 250
const PROGRAM_WINDOW_BEFORE_SECS = 60 * 60
const PROGRAM_WINDOW_AFTER_SECS = 4 * 60 * 60
const PROGRAM_WINDOW_STEP_SECS = 4 * 60 * 60

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
  0: 'white', 1: 'cyan', 2: 'white', 3: 'pink', 4: 'orange', 5: 'lime',
  6: 'brown', 7: 'yellow', 8: 'blue', 9: 'ochre', 10: 'teal', 11: 'white',
  14: 'white', 15: 'white',
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
  prefectureCode: number | null
}
type SubchannelInfo = { service: string; parent: string; distinct: boolean }
type Program = {
  id: number | null
  key: string
  nid: number
  sid: number
  event_id: number
  start_at: number
  duration_secs: number
  name: string
  description: string
  extended: string
  genre: number | null
  loaded?: boolean
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
  tsid: number
  sid: number
  remoteControlKey: number | null
  items: RenderItem[]
}
type ProgramWindow = [number, number]
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
  return value === null || value === undefined || [1, 2, 0xa1, 0xa2, 0xa5, 0xa6, 0xad].includes(Number(value))
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
  return level === null ? 'white' : (GENRE_COLORS[level] ?? 'white')
}
function fmtDateInput(date: Date): string {
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`
}
function fmtTime(epoch: number): string {
  const date = new Date(epoch * 1000)
  return `${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}`
}
function fmtMinute(epoch: number): string {
  return String(new Date(epoch * 1000).getMinutes()).padStart(2, '0')
}
function guideTimeVariable(epoch: number): string {
  return `var(--guide-time-${String(new Date(epoch * 1000).getHours()).padStart(2, '0')})`
}
function isPast(program: Program): boolean {
  return program.start_at + program.duration_secs <= Math.floor(now.value / 1000)
}
function isOnAir(program: Program): boolean {
  const current = Math.floor(now.value / 1000)
  return program.start_at <= current && current < program.start_at + program.duration_secs
}

/*
 * 本番の /programs は 2 万件を超える。ref だと配列の要素まで再帰的に Proxy 化され、
 * 分類ループが 2 万個 × 各プロパティぶんの依存を張って一気に重くなる。
 * 差し替えしか起きないので shallowRef で持つ (KonomiTV の TimeTableStore も
 * channels_data を shallowRef にしている)。
 */
const rawChannels = shallowRef<JsonRecord[]>([])
const rawPrograms = shallowRef<JsonRecord[]>([])
/**
 * 表示日に番組を持つサービス (`nid:sid`)。列はこれに載っているものだけにする。
 * channels は全国のスキャン結果 (受信できない地域を含む) なので、そのまま列にすると
 * 先頭が番組のない局で埋まり、番組のある局 (福島など) が画面外に追いやられる。
 * 番組取得は見えている列だけなので、先頭が全部空だと 0 件 → 空表示でグリッドごと
 * 消え、スクロールもできず二度と取りに行かなかった。EDCB の EpgTimer も
 * 「表示対象サービス ∩ EPG データのあるサービス」を列にしている (EpgViewBase.cs)。
 * null = 未取得/取得失敗 (旧サーバー) で、そのときは従来どおり全サービスを出す。
 */
const epgServiceKeys = shallowRef<Set<string> | null>(null)
const subchannelByService = shallowRef<Map<string, SubchannelInfo> | null>(null)
const subchannelParents = computed(() => new Set([...(subchannelByService.value?.values() ?? [])].map((item) => item.parent)))
const error = ref('')
const loading = ref(false)
const channelsLoading = ref(false)
const programsLoading = ref(false)
const loadedProgramWindows = shallowRef<Map<string, ProgramWindow[]>>(new Map())
const bandFilter = ref<'すべて' | BandCategory>('すべて')
const regionFilter = ref('すべて')
const regionFilterTouched = ref(false)
const serviceQuery = ref('')
/** 畳んだ中で既定から外れた絞り込みがあるか (閉じていても気付けるように印を付ける)。 */
const filtersActive = computed(() => regionFilter.value !== 'すべて' || serviceQuery.value.trim() !== '')
const now = ref(Date.now())
const selectedDate = ref(broadcastDateInput(now.value, GRID_START_HOUR))
const detail = ref<Program | null>(null)
const selected = ref<{ columnIndex: number; programId: string } | null>(null)
const selectedProgram = ref<Program | null>(null)
const popupProgram = ref<Program | null>(null)
const popupStyle = ref<Record<string, string>>({ visibility: 'hidden' })
const popupElement = ref<HTMLElement | null>(null)
let popupHideTimer: number | null = null
let popupAnchor: HTMLElement | null = null
const previewProgram = ref<Program | null>(null)
const epgStates = shallowRef<EpgScanState[]>([])
const epgTargetHours = ref(168)
const epgRefreshSecs = ref(86400)
const activeEpgStatusKey = ref<string | null>(null)
/** 狭幅で畳んだ表示設定・絞り込みを開いているか。 */
const filtersOpen = ref(false)
const scrollArea = ref<HTMLElement | null>(null)
const isNarrow = ref(false)
const viewportHeight = ref(0)
const visibleRangeTop = ref(0)
const visibleRangeBottom = ref(0)
const visibleColumnStart = ref(0)
const visibleColumnEnd = ref(0)
const viewportWidth = ref(0)
const channelCount = ref<number>(DEFAULT_CHANNEL_COUNT)
/* ユーザーが局数を自分で選んだか。選んでいない間は画面幅に合わせて既定を切り替える
   (390px で 7 局だと 1 列 51px しかなく局名が読めない)。 */
const channelCountExplicit = ref(false)
const displayHours = ref<number>(DEFAULT_HOURS)
const showSubchannels = ref(false)
let scrollAnimationId: number | null = null
let pendingScrollTop = 0
let pendingScrollLeft = 0
let clockTimer = 0
let narrowMedia: MediaQueryList | null = null
let programIndex = new Map<string, number>()
let pendingProgramEvents: EpgProgramEvent[] = []
let programMergeTimer: number | null = null
/** 応答待ちの取得範囲 (局ごと)。再描画に関わらないので reactive にしない。 */
const pendingProgramWindows = new Map<string, ProgramWindow[]>()
/** loadPrograms のたびに進める。古い応答を捨てるための世代番号。 */
let programGeneration = 0
let initialProgramsReady = false
let scrollResizeObserver: ResizeObserver | null = null
let detailLoadTimer: number | null = null

const pxPerMin = computed(() => Math.max(1, (viewportHeight.value - headerHeight.value) / (displayHours.value * 60)))
const columnWidth = computed(() => Math.max(40, (viewportWidth.value - axisWidth.value) / channelCount.value))
/** チャンネルヘッダー行の高さ。リモコン番号+ロゴ+局名の2段が収まる最小値。 */
const headerHeight = computed(() => (isNarrow.value ? 48 : 54))
const axisWidth = computed(() => (isNarrow.value ? AXIS_WIDTH_NARROW : AXIS_WIDTH_DESKTOP))
const totalHeight = computed(() => TOTAL_MINUTES * pxPerMin.value)
const visibleBufferPx = computed(
  () =>
    (isNarrow.value ? VISIBLE_BUFFER_MINUTES_NARROW : VISIBLE_BUFFER_MINUTES_DESKTOP) *
    pxPerMin.value,
)
const columnBuffer = computed(() => (isNarrow.value ? COLUMN_BUFFER_NARROW : COLUMN_BUFFER_DESKTOP))
const densityChannelOptions = computed(() => isNarrow.value ? CHANNEL_COUNT_OPTIONS.filter((count) => count < 9) : CHANNEL_COUNT_OPTIONS)

function loadDensity(): void {
  try {
    const stored = JSON.parse(localStorage.getItem(DENSITY_STORAGE_KEY) ?? '{}') as {
      channels?: unknown
      hours?: unknown
      subchannels?: unknown
    }
    if (CHANNEL_COUNT_OPTIONS.includes(Number(stored.channels) as 5 | 7 | 9)) {
      channelCount.value = Number(stored.channels)
      channelCountExplicit.value = true
    }
    if (HOURS_OPTIONS.includes(Number(stored.hours) as 3 | 4 | 6)) displayHours.value = Number(stored.hours)
    if (typeof stored.subchannels === 'boolean') showSubchannels.value = stored.subchannels
  } catch {
    channelCountExplicit.value = false
    channelCount.value = DEFAULT_CHANNEL_COUNT
    displayHours.value = DEFAULT_HOURS
    showSubchannels.value = false
  }
}
function saveDensity(): void {
  try {
    localStorage.setItem(DENSITY_STORAGE_KEY, JSON.stringify({
      channels: channelCount.value,
      hours: displayHours.value,
      subchannels: showSubchannels.value,
    }))
  } catch {
    // localStorage unavailable: the current selection remains usable in memory.
  }
}
function setChannelCount(value: number): void {
  if (densityChannelOptions.value.includes(value as 5 | 7 | 9)) {
    channelCount.value = value
    channelCountExplicit.value = true
    saveDensity()
  }
}
function setDisplayHours(value: number): void {
  if (HOURS_OPTIONS.includes(value as 3 | 4 | 6)) {
    displayHours.value = value
    saveDensity()
  }
}
function setShowSubchannels(value: boolean): void {
  showSubchannels.value = value
  saveDensity()
}

const gridStart = computed(() => {
  const [year, month, day] = selectedDate.value.split('-').map(Number)
  return new Date(year, month - 1, day, GRID_START_HOUR)
})
const gridBounds = computed(() => {
  const since = Math.floor(gridStart.value.getTime() / 1000)
  return { since, until: since + TOTAL_MINUTES * 60 }
})
const isToday = computed(() => selectedDate.value === broadcastDateInput(now.value, GRID_START_HOUR))
const BAND_TABS = ['地上', 'BS', 'CS', 'すべて'] as const
const WEEKDAYS = ['日', '月', '火', '水', '木', '金', '土']

const dateLabel = computed(() => {
  const [y, m, d] = selectedDate.value.split('-').map(Number)
  const date = new Date(y, m - 1, d)
  return `${m}/${d}(${WEEKDAYS[date.getDay()]})`
})
const nowOffset = computed(() => ((now.value / 1000 - gridBounds.value.since) / 60) * pxPerMin.value)
const nowLabel = computed(() => fmtTime(Math.floor(now.value / 1000)))
const showNowLine = computed(
  () => isToday.value && nowOffset.value >= 0 && nowOffset.value <= totalHeight.value,
)
const hourMarks = computed(() =>
  Array.from({ length: GRID_HOURS }, (_, index) => ({
    offset: index * 60 * pxPerMin.value,
    hour: (GRID_START_HOUR + index) % 24,
    label: `${String((GRID_START_HOUR + index) % 24).padStart(2, '0')}:00`,
    hourLabel: String((GRID_START_HOUR + index) % 24).padStart(2, '0'),
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
  updateVisibleRangeIfNeeded(pendingScrollTop - headerHeight.value)
  updateVisibleColumns(pendingScrollLeft, element.clientWidth)
  refreshProgramPopupPosition()
  void loadMoreForScroll()
}
async function loadMoreForScroll(): Promise<void> {
  const { since, until } = gridBounds.value
  await loadVisiblePrograms()
  if (pendingScrollLeft + (scrollArea.value?.clientWidth ?? 0) >
      axisWidth.value + (visibleColumnEnd.value - 2) * columnWidth.value) {
    await loadProgramsWindow(since, until)
  }
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
  refreshProgramPopupPosition()
}
/** 可視範囲を無条件で作り直す (初期表示・リサイズ・データ差し替え時)。 */
function resizeGrid(): void {
  const element = scrollArea.value
  isNarrow.value = narrowMedia?.matches ?? window.innerWidth <= 700
  if (channelCountExplicit.value) {
    // 狭幅に 9 局は入らない。明示選択でも 1 段落とす。
    if (isNarrow.value && channelCount.value === 9) {
      channelCount.value = DEFAULT_CHANNEL_COUNT
      saveDensity()
    }
  } else {
    channelCount.value = isNarrow.value ? DEFAULT_CHANNEL_COUNT_NARROW : DEFAULT_CHANNEL_COUNT
  }
  viewportWidth.value = element?.clientWidth ?? 0
  viewportHeight.value = element?.clientHeight ?? 0
  if (element && scrollResizeObserver) scrollResizeObserver.observe(element)
  pendingScrollTop = element?.scrollTop ?? 0
  pendingScrollLeft = element?.scrollLeft ?? 0
  updateVisibleRange(pendingScrollTop - headerHeight.value)
  updateVisibleColumns(pendingScrollLeft, element?.clientWidth ?? 0)
}

async function loadChannels() {
  if (channelsLoading.value) return
  channelsLoading.value = true
  try {
    const response = await api('/channels?group_logical=true')
    const rows = unwrapArray(response, ['channels'])
    rawChannels.value = rows
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    channelsLoading.value = false
  }
}
function visibleServiceQuery(serviceKeys = visibleServiceKeys()): string {
  return serviceKeys.join(',')
}
function visibleServiceKeys(): string[] {
  const start = Math.max(0, visibleColumnStart.value - columnBuffer.value)
  const end = Math.min(columns.value.length, visibleColumnEnd.value + columnBuffer.value)
  const keys = new Set<string>()
  for (const column of columns.value.slice(start, end)) {
    keys.add(column.key)
    const parent = subchannelByService.value?.get(column.key)?.parent
    if (parent) keys.add(parent)
  }
  return [...keys]
}
function windowLoaded(serviceKey: string, since: number, until: number): boolean {
  const covers = ([start, end]: ProgramWindow) => start <= since && end >= until
  return (loadedProgramWindows.value.get(serviceKey) ?? []).some(covers) ||
    (pendingProgramWindows.get(serviceKey) ?? []).some(covers)
}
async function loadProgramsWindow(since: number, until: number, force = false) {
  const visibleKeys = visibleServiceKeys()
  const requestedKeys = force
    ? visibleKeys
    : visibleKeys.filter((serviceKey) => !windowLoaded(serviceKey, since, until))
  if (!requestedKeys.length) return
  // 応答待ちの範囲も「取得済み」とみなす。列の入れ替えで同じ窓を二重に要求しない。
  const generation = programGeneration
  for (const serviceKey of requestedKeys) {
    pendingProgramWindows.set(serviceKey, [...(pendingProgramWindows.get(serviceKey) ?? []), [since, until]])
  }
  programsLoading.value = true
  try {
    const query = new URLSearchParams({ since: String(since), until: String(until), brief: 'true', limit: '20000' })
    const servicesQuery = visibleServiceQuery(requestedKeys)
    if (servicesQuery) query.set('services', servicesQuery)
    const rows = unwrapArray(await api(`/programs?${query}`), ['programs']).map(normalizeProgramRow)
    // 待っている間に日付変更・再取得 (loadPrograms) が走っていたら、古い応答は捨てる。
    if (generation !== programGeneration) return
    const keys = new Set(rawPrograms.value.map(programEventKey))
    rawPrograms.value = [...rawPrograms.value, ...rows.filter((row) => !keys.has(programEventKey(row)))]
    programIndex = buildProgramIndex(rawPrograms.value)
    const nextWindows = new Map(loadedProgramWindows.value)
    for (const serviceKey of requestedKeys) {
      const windows = nextWindows.get(serviceKey) ?? []
      nextWindows.set(serviceKey, [...windows, [since, until]])
    }
    loadedProgramWindows.value = nextWindows
    error.value = ''
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    if (generation === programGeneration) {
      for (const serviceKey of requestedKeys) {
        const rest = (pendingProgramWindows.get(serviceKey) ?? [])
          .filter(([start, end]) => start !== since || end !== until)
        if (rest.length) pendingProgramWindows.set(serviceKey, rest)
        else pendingProgramWindows.delete(serviceKey)
      }
    }
    programsLoading.value = false
  }
}
/**
 * 今見えている時間帯 (+ バッファ) を、見えている列のぶんだけ読む。取得済みの局は飛ばす。
 * スクロールだけでなく、列の顔ぶれが変わったとき (帯域タブ・地域・検索・サブCH・局数) にも
 * 呼ぶ。以前はスクロールでしか呼ばれず、帯域タブを押すと新しく並んだ局が空のまま
 * 指でスクロールするまで番組が出なかった。
 */
async function loadVisiblePrograms(): Promise<void> {
  if (!initialProgramsReady) return
  const top = Math.max(0, pendingScrollTop - headerHeight.value)
  const bottom = top + viewportHeight.value
  const { since, until } = gridBounds.value
  const programWindow = calculateGuideProgramWindow({
    top,
    bottom,
    gridSince: since,
    gridUntil: until,
    pxPerMin: pxPerMin.value,
    bufferPx: visibleBufferPx.value,
    stepSecs: PROGRAM_WINDOW_STEP_SECS,
  })
  if (programWindow !== null) await loadProgramsWindow(...programWindow)
}
async function loadInitialPrograms() {
  const { since: gridSince, until: gridUntil } = gridBounds.value
  const current = Math.floor(Date.now() / 1000)
  const since = isToday.value
    ? Math.max(gridSince, current - PROGRAM_WINDOW_BEFORE_SECS)
    : gridSince
  const until = isToday.value
    ? Math.min(gridUntil, current + PROGRAM_WINDOW_AFTER_SECS)
    : Math.min(gridUntil, since + PROGRAM_WINDOW_STEP_SECS)
  if (since >= until) return
  await loadProgramsWindow(since, until)
  await nextTick()
  if (isToday.value && scrollArea.value) {
    scrollArea.value.scrollTop = Math.max(0, nowOffset.value - viewportHeight.value * 0.25 + headerHeight.value)
    pendingScrollTop = scrollArea.value.scrollTop
    resizeGrid()
  }
}
async function loadEpgServices(): Promise<void> {
  const { since, until } = gridBounds.value
  try {
    const query = new URLSearchParams({ since: String(since), until: String(until) })
    const response = await api<JsonRecord>(`/programs/services?${query}`)
    const keys = Array.isArray(response.services) ? response.services.map(String) : null
    epgServiceKeys.value = keys ? new Set(keys) : null
    if (Array.isArray(response.subchannels)) {
      const map = new Map<string, SubchannelInfo>()
      for (const item of response.subchannels) {
        if (!item || typeof item !== 'object') continue
        const value = item as JsonRecord
        if (typeof value.service === 'string' && typeof value.parent === 'string') {
          map.set(value.service, { service: value.service, parent: value.parent, distinct: value.distinct === true })
        }
      }
      subchannelByService.value = map
    } else {
      subchannelByService.value = null
    }
    await applyGuideDefaultRegion()
  } catch {
    epgServiceKeys.value = null
    subchannelByService.value = null
    await applyGuideDefaultRegion()
  }
}
async function loadPrograms() {
  programGeneration += 1
  const generation = programGeneration
  initialProgramsReady = false
  pendingProgramWindows.clear()
  loadedProgramWindows.value = new Map()
  rawPrograms.value = []
  programIndex = new Map()
  // 列 (= 番組のある局) を先に確定させてから、見えている列の番組を読む。
  await loadEpgServices()
  if (generation !== programGeneration) return
  await nextTick()
  resizeGrid()
  await loadInitialPrograms()
  // 初回窓 (現在時刻の前後) を読み終えてから、列の変化に応じた追加取得を解禁する。
  // 先に解禁すると、スクロール位置が 06:00 のままの窓を取りに行って無駄になる。
  initialProgramsReady = true
  void loadVisiblePrograms()
}
async function refresh() {
  loading.value = true
  void loadChannels()
    .then(async () => {
      // チャンネル取得前は visibleColumnEnd=0 のため、番組APIへ services を渡せず
      // 本番の全サービス・数万件を初回取得していた。列範囲を確定してから読む。
      await nextTick()
      resizeGrid()
      await loadPrograms()
    })
    .finally(() => { loading.value = false })
  await nextTick()
  resizeGrid()
}

function flushProgramEvents(): void {
  programMergeTimer = null
  if (!pendingProgramEvents.length) return
  const rows = rawPrograms.value.slice()
  const index = new Map(programIndex)
  for (const event of pendingProgramEvents) {
    const program = event.program
    const serviceKey = program ? `${Number(program.nid)}:${Number(program.sid)}` : null
    // 収集中に初めて番組が届いた局は列に加える (更新ボタンを押さなくても出る)。
    const known = epgServiceKeys.value
    if (serviceKey && known && !known.has(serviceKey)) epgServiceKeys.value = new Set(known).add(serviceKey)
    mergeProgramEvent(rows, index, event, serviceKey ? loadedProgramWindows.value.get(serviceKey) ?? [] : [])
  }
  pendingProgramEvents = []
  rawPrograms.value = rows
  programIndex = index
}
function queueProgramEvent(event: EpgProgramEvent): void {
  pendingProgramEvents.push(event)
  if (programMergeTimer === null) {
    programMergeTimer = window.setTimeout(flushProgramEvents, 300)
  }
}
function replaceEpgStates(states: EpgScanState[]): void {
  epgStates.value = states
}
function statusForColumn(column: GuideColumn) {
  const state = epgStates.value.find(
    (item) => item.network_id === column.nid && item.tsid === column.tsid,
  )
  return epgStatusPresentation(state, Math.floor(Date.now() / 1000), epgTargetHours.value, epgRefreshSecs.value)
}
async function loadEpgStatus(): Promise<void> {
  try {
    const [status, effective] = await Promise.all([
      api<JsonRecord>('/epg/status'),
      api<JsonRecord>('/epg-effective'),
    ])
    if (Array.isArray(status.states)) replaceEpgStates(status.states as EpgScanState[])
    const config = (effective.effective ?? effective) as JsonRecord
    const target = Number(config.target_future_coverage_hours)
    const refresh = Number(config.target_refresh_secs)
    if (Number.isFinite(target) && target > 0) epgTargetHours.value = target
    if (Number.isFinite(refresh) && refresh > 0) epgRefreshSecs.value = refresh
  } catch {
    // EPG status は番組表本体を止めない。
  }
}
const epgEvents = useEpgEvents({
  onProgram: queueProgramEvent,
  onLagged: () => { void loadPrograms() },
  onStatus: replaceEpgStates,
  onReconnect: () => { void loadPrograms() },
})

/**
 * 並び: 帯域 → (地上) 都道府県コード → チャンネル番号 (EDCB 規則) → nid → sid。
 * 番号は同じ NID の中の順位で決まるので、比較のたびに全サービスを舐めず先に一度だけ求める
 * (本番は 800 サービス前後。比較ごとに求めると数千万回の走査になる)。
 */
function sortServices(all: Service[]): Service[] {
  const byNid = new Map<number, Service[]>()
  for (const service of all) {
    const list = byNid.get(service.nid)
    if (list) list.push(service)
    else byNid.set(service.nid, [service])
  }
  const numbers = new Map(all.map((service) => [service.key, guideChannelNumber(service, byNid.get(service.nid) ?? [])]))
  return all.sort((a, b) =>
    BAND_ORDER[a.band] - BAND_ORDER[b.band]
    || (a.band === '地上' ? (a.prefectureCode ?? 255) - (b.prefectureCode ?? 255) : 0)
    || (numbers.get(a.key) ?? 0) - (numbers.get(b.key) ?? 0)
    || a.nid - b.nid || a.sid - b.sid)
}

const services = computed<Service[]>(() => {
  const seen = new Map<string, Service>()
  const withEpg = epgServiceKeys.value
  for (const row of rawChannels.value) {
    const nid = Number(row.nid),
      sid = Number(row.sid),
      tsid = Number(row.tsid)
    if (
      ![nid, sid, tsid].every(Number.isFinite)
      || !isRealGuideService({ sid, tsid })
      || !isGuideServiceType(row.service_type)
    ) continue
    const key = `${nid}:${sid}`
    const sub = subchannelByService.value?.get(key)
    if (seen.has(key) || (withEpg && !withEpg.has(key) && !sub && !subchannelParents.value.has(key))) continue
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
      prefectureCode: row.prefecture_code == null ? null : Number(row.prefecture_code),
    })
  }
  return sortServices([...seen.values()])
})
const mainSidByGroup = computed(() => {
  const result = new Map<string, number>()
  for (const service of mainGuideServices(services.value)) {
    result.set(guideServiceGroupKey(service), service.sid)
  }
  return result
})
const logoFallback = ref(new Map<string, 'own' | 'main' | 'hidden'>())
function logoSrc(column: GuideColumn): string {
  const id = column.key
  const state = logoFallback.value.get(id) ?? 'own'
  if (state === 'hidden') return ''
  if (state === 'main') {
    const sid = mainSidByGroup.value.get(guideServiceGroupKey(column))
    if (sid === undefined || sid === column.sid) return ''
    return `/logos/${column.nid}_${sid}.png`
  }
  return `/logos/${column.nid}_${column.sid}.png`
}
function onLogoError(column: GuideColumn): void {
  const state = logoFallback.value.get(column.key) ?? 'own'
  logoFallback.value.set(column.key, state === 'own' ? 'main' : 'hidden')
}
function channelColorStyle(column: GuideColumn): string {
  const paletteIndex = column.remoteControlKey !== null && column.remoteControlKey >= 1 && column.remoteControlKey <= 12
    ? column.remoteControlKey
    : ((Math.abs(column.nid * 31 + column.sid) % 12) + 1)
  return `var(--guide-ch-${paletteIndex})`
}
const regionOptions = computed(() => [...new Map(
  services.value.filter((service) => service.band === '地上' && service.region)
    .map((service) => [service.region as string, service.prefectureCode ?? 255]),
)].sort((a, b) => a[1] - b[1] || a[0].localeCompare(b[0])).map(([name]) => name))

async function applyGuideDefaultRegion(): Promise<void> {
  if (regionFilterTouched.value) return
  try {
    const config = await api<JsonRecord>('/guide-config')
    const configured = config.default_region
    if (configured === '*') regionFilter.value = 'すべて'
    else if (typeof configured === 'string' && regionOptions.value.includes(configured)) regionFilter.value = configured
    else regionFilter.value = regionOptions.value[0] ?? 'すべて'
  } catch {
    regionFilter.value = regionOptions.value[0] ?? 'すべて'
  }
}
watch(bandFilter, (value) => {
  if (value !== '地上' && value !== 'すべて') regionFilter.value = 'すべて'
})
watch(regionFilter, (value) => {
  if (value !== 'すべて' && bandFilter.value !== '地上') bandFilter.value = '地上'
})
function onRegionChange(): void {
  regionFilterTouched.value = true
}
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
const displayedServices = computed(() => {
  if (!subchannelByService.value) {
    if (showSubchannels.value) return filteredServices.value
    return filteredServices.value.filter((service) => mainSidByGroup.value.get(guideServiceGroupKey(service)) === service.sid)
  }
  const filtered = filteredServices.value.filter((service) => {
    const sub = subchannelByService.value?.get(service.key)
    return showSubchannels.value || !sub || sub.distinct
  })
  // サブは親のすぐ右。キー = (親の順位, サブか, 自分の順位) で全順序にする。
  const order = serviceOrder.value
  const subs = subchannelByService.value
  const rank = (service: Service): [number, number, number] => {
    const own = order.get(service.key) ?? Number.MAX_SAFE_INTEGER
    const parent = subs?.get(service.key)?.parent
    return parent === undefined ? [own, 0, own] : [order.get(parent) ?? own, 1, own]
  }
  return filtered
    .map((service) => ({ service, key: rank(service) }))
    .sort((a, b) => a.key[0] - b.key[0] || a.key[1] - b.key[1] || a.key[2] - b.key[2])
    .map((entry) => entry.service)
})
const serviceOrder = computed(() => new Map(services.value.map((service, index) => [service.key, index])))
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
      id: Number.isFinite(Number(row.id)) ? Number(row.id) : null,
      key: programEventKey(row),
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
  const result: GuideColumn[] = []
  for (const service of displayedServices.value) {
    const key = service.key
    const programs = byService.get(service.key) ?? []
    const sub = subchannelByService.value?.get(service.key)
    const parentPrograms = sub ? byService.get(sub.parent) ?? [] : []
    const items: RenderItem[] = []
    const push = (program: Program): void => {
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
          left: '0',
          width: '100%',
          borderLeftColor: color,
          '--guide-genre-highlight': `var(--guide-genre-${color}-highlight)`,
          '--guide-genre-background': `var(--guide-genre-${color}-background)`,
          '--guide-hour-tint': guideTimeVariable(program.start_at),
          '--guide-cell-text': 'var(--text)',
        },
      })
    }
    for (const program of programs) {
      if (!showSubchannels.value && sub && (!sub.distinct || isPlaceholderProgramName(program.name)
        || parentPrograms.some((parent) => parent.start_at === program.start_at
          && parent.name.trim() === program.name.trim()))) continue
      push(program)
    }
    // 上から順に並べておくと、可視判定を先頭から走らせて途中で打ち切れる。
    items.sort((a, b) => a.top - b.top)
    result.push({
      key,
      name: service.name,
      subLabel: sub ? 'サブ' : '',
      band: service.band,
      nid: service.nid,
      tsid: service.tsid,
      sid: service.sid,
      remoteControlKey: service.remoteControlKey,
      items,
    })
  }
  // displayedServices の順 (チャンネル番号順・サブは親の右) をそのまま使う。
  return result
})
const activeEpgStatusText = computed(() => {
  if (activeEpgStatusKey.value === null) return ''
  const column = columns.value.find((item) => item.key === activeEpgStatusKey.value)
  return column ? `${column.name}: ${statusForColumn(column).text}` : ''
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
/** セルを選ぶ。列の添字を持っておくと左右移動が O(1) で決まる。 */
function selectProgram(columnIndex: number, program: Program): void {
  selected.value = { columnIndex, programId: program.key }
  selectedProgram.value = program
  // 一覧の行には説明文が入っていない。詳細ペインに出すぶんだけ後から取りに行く。
  // 十字キーを押しっぱなしにすると通過した番組ぶん要求が飛ぶので、止まってから引く。
  if (detailLoadTimer !== null) window.clearTimeout(detailLoadTimer)
  if (program.loaded) return
  detailLoadTimer = window.setTimeout(() => {
    detailLoadTimer = null
    if (selectedProgram.value?.key === program.key) void loadProgramDetail(program)
  }, DETAIL_LOAD_DEBOUNCE_MS)
}

function clearPopupHideTimer(): void {
  if (popupHideTimer !== null) window.clearTimeout(popupHideTimer)
  popupHideTimer = null
}

function positionProgramPopup(): void {
  const anchor = popupAnchor
  const popup = popupElement.value
  if (anchor === null || popup === null) return
  const anchorRect = anchor.getBoundingClientRect()
  const popupRect = popup.getBoundingClientRect()
  const viewport = window.visualViewport
  const viewportLeft = viewport?.offsetLeft ?? 0
  const viewportTop = viewport?.offsetTop ?? 0
  const viewportWidth = viewport?.width ?? document.documentElement.clientWidth
  const viewportHeight = viewport?.height ?? document.documentElement.clientHeight
  const gap = 8
  const margin = 8
  let left = anchorRect.right + gap
  if (left + popupRect.width > viewportLeft + viewportWidth - margin) {
    left = anchorRect.left - popupRect.width - gap
  }
  left = Math.max(viewportLeft + margin, Math.min(left, viewportLeft + viewportWidth - popupRect.width - margin))
  let top = anchorRect.top
  if (top + popupRect.height > viewportTop + viewportHeight - margin) {
    top = anchorRect.bottom - popupRect.height
  }
  top = Math.max(viewportTop + margin, Math.min(top, viewportTop + viewportHeight - popupRect.height - margin))
  popupStyle.value = { left: `${left}px`, top: `${top}px`, visibility: 'visible' }
}

function findProgramAnchor(program: Program): HTMLElement | null {
  return [...(scrollArea.value?.querySelectorAll<HTMLElement>('[data-program-key]') ?? [])]
    .find((element) => element.dataset.programKey === program.key) ?? null
}

/** 再描画でセルが差し替わってもアンカーを引き直してポップアップを追従させる。 */
function refreshProgramPopupPosition(): void {
  const program = popupProgram.value
  if (program === null) return
  void nextTick().then(() => {
    if (popupProgram.value?.key !== program.key) return
    const anchor = findProgramAnchor(program)
    if (anchor === null) {
      closeProgramPopup()
      return
    }
    popupAnchor = anchor
    positionProgramPopup()
  })
}

function showProgramPopup(program: Program, anchor: HTMLElement | null = null): void {
  clearPopupHideTimer()
  popupProgram.value = program
  if (anchor !== null) popupAnchor = anchor
  void nextTick().then(positionProgramPopup)
}

function showSelectedPopup(): void {
  const program = selectedProgram.value
  if (program === null) return
  void nextTick().then(() => {
    const anchor = findProgramAnchor(program)
    showProgramPopup(program, anchor)
  })
}

function schedulePopupHide(): void {
  clearPopupHideTimer()
  popupHideTimer = window.setTimeout(() => {
    popupHideTimer = null
    if (!popupElement.value?.matches(':hover')) closeProgramPopup()
  }, 180)
}

function closeProgramPopup(): void {
  clearPopupHideTimer()
  popupProgram.value = null
  popupAnchor = null
  popupStyle.value = { visibility: 'hidden' }
}

function onProgramEnter(event: MouseEvent, columnIndex: number, program: Program): void {
  selectProgram(columnIndex, program)
  showProgramPopup(program, event.currentTarget as HTMLElement)
}

function onProgramClick(event: MouseEvent, columnIndex: number, program: Program): void {
  selectProgram(columnIndex, program)
  showProgramPopup(program, event.currentTarget as HTMLElement)
}

function onProgramFocus(event: FocusEvent, program: Program): void {
  showProgramPopup(program, event.currentTarget as HTMLElement)
}

/** 列の中で、指定した時刻を含む(なければ直後の)番組を返す。items は top 昇順。 */
function itemNearTime(column: GuideColumn, startAt: number): Program | null {
  let candidate: Program | null = null
  for (const item of column.items) {
    if (item.program.start_at <= startAt) candidate = item.program
    else if (candidate === null) return item.program
    else break
  }
  return candidate
}

/** 選択中セルが可視範囲に入るまでスクロールする。DOM 計測はせず、事前計算した top を使う。 */
function revealSelected(columnIndex: number, program: Program): void {
  const element = scrollArea.value
  if (element === null) return
  const top = Math.max(0, (program.start_at - gridBounds.value.since) / 60) * pxPerMin.value
  const header = headerHeight.value
  const viewTop = element.scrollTop
  const viewBottom = viewTop + element.clientHeight - header
  if (top < viewTop) element.scrollTop = Math.max(0, top - 40)
  else if (top + 40 > viewBottom) element.scrollTop = top - element.clientHeight + header + 80
  const left = axisWidth.value + columnIndex * columnWidth.value
  if (left < element.scrollLeft + axisWidth.value) element.scrollLeft = Math.max(0, left - axisWidth.value)
  else if (left + columnWidth.value > element.scrollLeft + element.clientWidth) {
    element.scrollLeft = left + columnWidth.value - element.clientWidth
  }
  pendingScrollTop = element.scrollTop
  pendingScrollLeft = element.scrollLeft
  scheduleScrollUpdate()
}

function moveSelection(dx: number, dy: number): void {
  const all = columns.value
  if (all.length === 0) return
  const current = selected.value
  if (current === null) {
    const column = all[Math.max(0, visibleColumnStart.value)]
    const first = column?.items[0]?.program
    if (first) {
      selectProgram(Math.max(0, visibleColumnStart.value), first)
      revealSelected(Math.max(0, visibleColumnStart.value), first)
    }
    return
  }
  const columnIndex = Math.min(Math.max(current.columnIndex + dx, 0), all.length - 1)
  const column = all[columnIndex]
  if (column === undefined) return
  if (dx !== 0) {
    const anchor = selectedProgram.value?.start_at ?? gridBounds.value.since
    const next = itemNearTime(column, anchor)
    if (next === null) return
    selectProgram(columnIndex, next)
    revealSelected(columnIndex, next)
    return
  }
  const at = column.items.findIndex((item) => item.program.key === current.programId)
  if (at < 0) return
  const target = column.items[at + dy]
  if (target === undefined) return
  selectProgram(columnIndex, target.program)
  revealSelected(columnIndex, target.program)
}

function onGridKeydown(event: KeyboardEvent): void {
  if (detail.value !== null || previewProgram.value !== null) return
  const target = event.target
  // 検索欄など、入力中のキー操作は奪わない。
  if (target instanceof HTMLElement && /^(INPUT|SELECT|TEXTAREA)$/.test(target.tagName)) return
  switch (event.key) {
    case 'ArrowUp': moveSelection(0, -1); break
    case 'ArrowDown': moveSelection(0, 1); break
    case 'ArrowLeft': moveSelection(-1, 0); break
    case 'ArrowRight': moveSelection(1, 0); break
    case 'Enter':
      if (selectedProgram.value !== null) openDetail(selectedProgram.value)
      else return
      break
    default: return
  }
  if (event.key.startsWith('Arrow')) showSelectedPopup()
  event.preventDefault()
}
function shiftDate(days: number) {
  const [y, m, d] = selectedDate.value.split('-').map(Number)
  const date = new Date(y, m - 1, d)
  date.setDate(date.getDate() + days)
  selectedDate.value = fmtDateInput(date)
}
function goToday() {
  selectedDate.value = broadcastDateInput(now.value, GRID_START_HOUR)
}
/** 今日へ移動し、現在時刻がビューポート中央付近に来るまで縦スクロールする。 */
function scrollToNow() {
  // 日付が変わる場合は watch(selectedDate) → loadPrograms → loadInitialPrograms が
  // 現在時刻付近まで寄せるので、ここでは日付を変えるだけにする。
  if (!isToday.value) {
    goToday()
    return
  }
  void nextTick().then(() => {
    const element = scrollArea.value
    if (element === null) return
    element.scrollTop = Math.max(0, nowOffset.value - element.clientHeight * 0.4 + headerHeight.value)
    pendingScrollTop = element.scrollTop
    resizeGrid()
  })
}
/** 一覧に無い説明文・拡張情報を 1 番組ぶんだけ取り、開いている詳細と選択中に反映する。 */
async function loadProgramDetail(program: Program): Promise<void> {
  if (program.loaded) return
  const since = Math.max(gridBounds.value.since, program.start_at - 60)
  const until = Math.min(gridBounds.value.until, program.start_at + Math.max(program.duration_secs, 60))
  try {
    const query = new URLSearchParams({ since: String(since), until: String(until), services: `${program.nid}:${program.sid}`, limit: '100' })
    const rows = unwrapArray(await api(`/programs?${query}`), ['programs'])
    const full = rows.find((row) => programEventKey(row) === program.key)
    if (!full) return
    const updated = { ...program, description: String(full.description ?? ''), extended: String(full.extended ?? ''), loaded: true }
    rawPrograms.value = rawPrograms.value.map((row) => programEventKey(row) === program.key ? { ...row, ...full, key: program.key } : row)
    programIndex = buildProgramIndex(rawPrograms.value)
    if (detail.value?.key === program.key) detail.value = updated
    if (selectedProgram.value?.key === program.key) selectedProgram.value = updated
    if (popupProgram.value?.key === program.key) popupProgram.value = updated
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}
function openDetail(program: Program) {
  closeProgramPopup()
  detail.value = program
  void loadProgramDetail(program)
}
function closeDetail() {
  detail.value = null
}
function openPreview(program: Program) {
  closeProgramPopup()
  detail.value = null
  previewProgram.value = program
}
function closePreview() {
  previewProgram.value = null
}
function onKeydown(event: KeyboardEvent) {
  if (event.key !== 'Escape') return
  if (previewProgram.value) closePreview()
  else if (detail.value) closeDetail()
  else if (popupProgram.value) closeProgramPopup()
  else {
    selected.value = null
    selectedProgram.value = null
  }
}
watch(selectedDate, () => {
  closeProgramPopup()
  void loadPrograms()
})
// 絞り込みや表示密度で列数が変わったら、可視範囲を取り直す。
watch([columns, pxPerMin], () => void nextTick().then(() => {
  resizeGrid()
  refreshProgramPopupPosition()
  // 新しく見えた局の番組を取る。取得済みなら要求は飛ばない (番組の到着で columns が
  // 変わってもここへ来るが、同じ窓は windowLoaded で弾かれるので往復しない)。
  void loadVisiblePrograms()
}))
onMounted(() => {
  loadDensity()
  narrowMedia = window.matchMedia(NARROW_MEDIA_QUERY)
  narrowMedia.addEventListener('change', resizeGrid)
  scrollResizeObserver = new ResizeObserver(() => resizeGrid())
  void refresh()
  void loadEpgStatus()
  epgEvents.start()
  resizeGrid()
  clockTimer = window.setInterval(() => {
    now.value = Date.now()
  }, 30000)
  window.addEventListener('resize', resizeGrid)
  window.visualViewport?.addEventListener('resize', positionProgramPopup)
  window.visualViewport?.addEventListener('scroll', refreshProgramPopupPosition)
  window.addEventListener('keydown', onKeydown)
})
onUnmounted(() => {
  window.clearInterval(clockTimer)
  narrowMedia?.removeEventListener('change', resizeGrid)
  window.removeEventListener('resize', resizeGrid)
  window.visualViewport?.removeEventListener('resize', positionProgramPopup)
  window.visualViewport?.removeEventListener('scroll', refreshProgramPopupPosition)
  window.removeEventListener('keydown', onKeydown)
  scrollResizeObserver?.disconnect()
  scrollResizeObserver = null
  if (scrollAnimationId !== null) cancelAnimationFrame(scrollAnimationId)
  if (programMergeTimer !== null) window.clearTimeout(programMergeTimer)
  programMergeTimer = null
  if (detailLoadTimer !== null) window.clearTimeout(detailLoadTimer)
  detailLoadTimer = null
  clearPopupHideTimer()
  pendingProgramEvents = []
  epgEvents.stop()
})
</script>

<template>
  <section class="view guide-view">
    <div class="guide-topbar">
      <h2 class="guide-title">番組表</h2>
      <div class="guide-date-nav">
        <button class="guide-icon-button" aria-label="前日" @click="shiftDate(-1)">◀</button>
        <div class="guide-date-picker">
          <span class="guide-date-label" aria-hidden="true" v-text="dateLabel" />
          <input v-model="selectedDate" type="date" aria-label="日付を選択" />
        </div>
        <button class="guide-icon-button" aria-label="翌日" @click="shiftDate(1)">▶</button>
        <button class="guide-chip-button" :class="{ active: isToday }" @click="goToday">今日</button>
        <button class="guide-chip-button" aria-label="現在時刻へ" @click="scrollToNow">現在時刻へ</button>
      </div>
      <div class="guide-band-tabs" role="group" aria-label="放送種別">
        <button
          v-for="tab in BAND_TABS"
          :key="tab"
          type="button"
          class="guide-band-tab"
          :class="{ active: bandFilter === tab }"
          :aria-pressed="bandFilter === tab"
          @click="bandFilter = tab; regionFilterTouched = true"
          v-text="tab === '地上' ? '地上' : tab"
        />
      </div>
      <!-- 狭幅では表示設定と絞り込みを畳む (縦画面でツールバーが 4 段・約180px になり、
           番組表が画面の半分しか使えなかった)。広い画面では display: contents で素通し。 -->
      <button
        type="button"
        class="guide-chip-button guide-filters-toggle"
        :class="{ active: filtersOpen || filtersActive }"
        :aria-expanded="filtersOpen"
        aria-controls="guide-filters"
        @click="filtersOpen = !filtersOpen"
        v-text="filtersActive ? '表示設定●' : '表示設定'"
      />
      <div id="guide-filters" class="guide-filters" :class="{ open: filtersOpen }">
      <div class="guide-density" role="group" aria-label="表示局数">
        <span class="guide-density-label">局数</span>
        <button
          v-for="count in densityChannelOptions"
          :key="`channels-${count}`"
          type="button"
          class="guide-density-button"
          :class="{ active: channelCount === count }"
          :aria-pressed="channelCount === count"
          @click="setChannelCount(count)"
          v-text="`${count}局`"
        />
      </div>
      <div class="guide-density" role="group" aria-label="表示時間幅">
        <span class="guide-density-label">時間</span>
        <button
          v-for="hours in HOURS_OPTIONS"
          :key="`hours-${hours}`"
          type="button"
          class="guide-density-button"
          :class="{ active: displayHours === hours }"
          :aria-pressed="displayHours === hours"
          @click="setDisplayHours(hours)"
          v-text="`${hours}時間`"
        />
      </div>
      <div class="guide-density" role="group" aria-label="サブチャンネル表示">
        <span class="guide-density-label">サブCH</span>
        <button
          type="button"
          class="guide-density-button"
          :class="{ active: showSubchannels }"
          :aria-pressed="showSubchannels"
          @click="setShowSubchannels(!showSubchannels)"
          v-text="showSubchannels ? 'オン' : 'オフ'"
        />
      </div>
      <label class="guide-region-filter">
        <span class="visually-hidden">地域（地上）</span>
        <select v-model="regionFilter" :disabled="!regionOptions.length" @change="onRegionChange">
          <option value="すべて">すべての地域</option>
          <option v-for="region in regionOptions" :key="region" :value="region" v-text="region" />
        </select>
      </label>
      <label class="guide-service-search">
        <span class="visually-hidden">サービス絞り込み</span>
        <input v-model="serviceQuery" type="search" placeholder="局名 / NID / SID" />
      </label>
      </div>
      <div class="guide-topbar-actions">
        <button class="guide-chip-button" @click="refresh" v-text="loading ? '更新中…' : '更新'" />
      </div>
    </div>
    <p v-if="error" class="notice error" role="alert" v-text="error" />
    <p v-if="!services.length && !loading" class="empty-state">
      番組情報がありません。番組情報は視聴中のチャンネルから自動収集されます。
    </p>
    <div v-else class="guide-layout">
      <div ref="scrollArea" class="guide-scroll" tabindex="0" @scroll.passive="onScroll" @keydown="onGridKeydown">
        <div
          class="guide-grid"
          :style="{
            width: `${axisWidth + columns.length * columnWidth}px`,
            height: `${totalHeight + headerHeight}px`,
            '--guide-col-w': `${columnWidth}px`,
            '--guide-hour-h': `${60 * pxPerMin}px`,
          }"
        >
        <div class="guide-header-row" :style="{ height: `${headerHeight}px` }">
          <div class="guide-corner" :style="{ width: `${axisWidth}px` }" />
          <div
            v-for="entry in visibleColumns"
            :key="`h-${entry.column.key}`"
            class="guide-header-cell"
            :style="{ left: `${axisWidth + entry.index * columnWidth}px`, width: `${columnWidth}px`, '--guide-channel-color': channelColorStyle(entry.column) }"
          >
            <div class="guide-header-top">
              <span
                v-if="entry.column.remoteControlKey !== null"
                class="guide-ch-num"
                v-text="entry.column.remoteControlKey"
              />
            <img
              v-if="logoSrc(entry.column)"
              class="guide-channel-logo"
              loading="lazy"
              decoding="async"
              :src="logoSrc(entry.column)"
              :alt="entry.column.name"
              @error="onLogoError(entry.column)"
            />
              <span class="guide-epg-status">
                <button
                  type="button"
                  class="guide-epg-status-dot"
                  :class="`guide-epg-status-${statusForColumn(entry.column).kind}`"
                  :title="statusForColumn(entry.column).text"
                  :aria-label="statusForColumn(entry.column).text"
                  @click.stop="activeEpgStatusKey = activeEpgStatusKey === entry.column.key ? null : entry.column.key"
                >●</button>
              </span>
            </div>
            <span class="guide-ch-name" v-text="entry.column.name" /><small
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
              :key="mark.offset"
              class="guide-hour-label"
              :style="{ top: `${mark.offset}px` }"
              :data-hour="mark.hour"
              v-text="mark.hourLabel"
            />
          </div>
          <div
            v-if="showNowLine"
            class="guide-now-line"
            :style="{ top: `${nowOffset}px`, left: `${axisWidth}px` }"
          ><span class="guide-now-badge" v-text="nowLabel" /></div>
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
              :key="item.program.key"
              type="button"
              class="guide-cell"
              :data-program-key="item.program.key"
              :class="{
                'guide-cell-past': isPast(item.program),
                'guide-cell-onair': isOnAir(item.program),
                'guide-cell-selected': selected?.programId === item.program.key,
              }"
              :aria-label="item.program.name || '番組名なし'"
              :aria-current="isOnAir(item.program) ? 'true' : undefined"
              :style="item.style"
              @mouseenter="onProgramEnter($event, entry.index, item.program)"
              @mouseleave="schedulePopupHide"
              @focus="onProgramFocus($event, item.program)"
              @click="onProgramClick($event, entry.index, item.program)"
            >
              <span class="guide-cell-highlight" aria-hidden="true" /><div class="guide-cell-content">
                <span class="guide-cell-head">
                  <span class="guide-cell-time" v-text="fmtMinute(item.program.start_at)" /><strong
                    v-if="item.program.name"
                    v-text="item.program.name"
                  /><span v-else class="guide-untitled">番組名なし</span>
                </span>
                <span
                v-if="item.program.description"
                class="guide-cell-description"
                v-text="item.program.description"
              /></div>
            </button>
          </div>
        </div>
        </div>
        <p v-if="!columns.length" class="empty-state">条件に一致するサービスがありません</p>
      </div>
      <section
        v-if="popupProgram"
        ref="popupElement"
        class="guide-program-popup"
        :style="popupStyle"
        role="dialog"
        aria-label="番組情報"
        @mouseenter="clearPopupHideTimer"
        @mouseleave="schedulePopupHide"
      >
        <span class="guide-detail-pane-kicker">番組情報</span>
        <h2 v-text="popupProgram.name || '番組情報'" />
        <p class="guide-detail-time">
          <span v-text="fmtTime(popupProgram.start_at)" />〜<span v-text="fmtTime(popupProgram.start_at + popupProgram.duration_secs)" />
        </p>
        <p class="guide-detail-genre">ジャンル: <span class="genre-badge" v-text="genreLabel(popupProgram.genre)" /></p>
        <div class="guide-detail-pane-copy">
          <p v-if="!popupProgram.loaded" class="muted">説明を読み込み中…</p>
          <p v-if="popupProgram.description" class="preserve-lines" v-text="popupProgram.description" />
          <p v-if="popupProgram.extended" class="preserve-lines guide-detail-extended" v-text="popupProgram.extended" />
          <p v-if="popupProgram.loaded && !popupProgram.description && !popupProgram.extended" class="muted">説明なし</p>
        </div>
        <div class="guide-actionbar">
          <button class="guide-chip-button" @click="openDetail(popupProgram)">番組詳細</button>
          <button class="guide-chip-button" @click="openPreview(popupProgram)">視聴</button>
        </div>
      </section>
    </div>
    <p v-if="activeEpgStatusText" class="notice guide-epg-status-notice" role="status" v-text="activeEpgStatusText" />
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

<style scoped>
/* ヘッダー右上に重ねる。フローに置くとタップ領域の高さで上段が膨らみ、
   狭幅 (ヘッダー 48px) では局名が押し出されて見えなくなる。
   見た目は小さく、タップ領域は ::before で広げる。 */
.guide-epg-status {
  position: absolute;
  top: 2px;
  right: 2px;
  z-index: 1;
  display: inline-flex;
}

.guide-epg-status-dot {
  position: relative;
  width: 14px;
  height: 14px;
  min-width: 0;
  min-height: 0;
  padding: 0;
  border: 0;
  color: var(--muted);
  background: transparent;
  cursor: pointer;
  font-size: .7rem;
  line-height: 14px;
}

.guide-epg-status-dot::before {
  position: absolute;
  inset: -10px;
  content: '';
}

.guide-epg-status-complete { color: var(--success); }
.guide-epg-status-scanning { color: var(--accent); }
.guide-epg-status-partial { color: var(--warning); }
.guide-epg-status-stale { color: var(--warning); }
.guide-epg-status-none { color: var(--muted); }
.guide-epg-status-failed { color: var(--danger); }

</style>

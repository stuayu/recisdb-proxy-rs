<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { api, unwrapArray, type JsonRecord } from '../api'
import PreviewPlayer from './PreviewPlayer.vue'

// --- constants -------------------------------------------------------

/** Pixels per minute of program duration. 24h grid == 1440 * PX_PER_MIN tall. */
const PX_PER_MIN = 2
const GRID_START_HOUR = 6
const TOTAL_MINUTES = 24 * 60
const TOTAL_HEIGHT = TOTAL_MINUTES * PX_PER_MIN

/** ARIB content_nibble level-1 category labels (major genre). */
const GENRE_LABELS: Record<number, string> = {
  0x0: 'ニュース/報道',
  0x1: 'スポーツ',
  0x2: '情報/ワイドショー',
  0x3: 'ドラマ',
  0x4: '音楽',
  0x5: 'バラエティ',
  0x6: '映画',
  0x7: 'アニメ/特撮',
  0x8: 'ドキュメンタリー/教養',
  0x9: '劇場/公演',
  0xa: '趣味/教育',
  0xb: '福祉',
  0xe: '拡張',
  0xf: 'その他',
}

const GENRE_COLORS: Record<number, string> = {
  0x0: '#3b82f6',
  0x1: '#22c55e',
  0x2: '#f59e0b',
  0x3: '#ec4899',
  0x4: '#a855f7',
  0x5: '#f97316',
  0x6: '#ef4444',
  0x7: '#06b6d4',
  0x8: '#14b8a6',
  0x9: '#eab308',
  0xa: '#84cc16',
  0xb: '#64748b',
  0xe: '#6366f1',
  0xf: '#9ca3af',
}

type BandCategory = '地上' | 'BS' | 'CS' | 'その他'

const BAND_ORDER: Record<BandCategory, number> = { 地上: 0, BS: 1, CS: 2, その他: 3 }

/**
 * Mirrors recisdb-proxy database/schema.rs BandType enum values.
 * Older scan rows can have `band_type = null`; fall back to deriving the
 * band from the NID range in that case (see docs/DESIGN.md NID ranges).
 */
function bandCategory(bandType: unknown, nid: number): BandCategory {
  if (bandType !== null && bandType !== undefined) {
    const value = Number(bandType)
    if (Number.isFinite(value)) {
      switch (value) {
        case 0: // Terrestrial
        case 5: // CATV
          return '地上'
        case 1: // BS
        case 3: // 4K (mirakurun maps this to BS too)
          return 'BS'
        case 2: // CS (110 degree)
        case 6: // SKY
          return 'CS'
        default:
          return 'その他'
      }
    }
  }
  if (nid === 4) return 'BS'
  if (nid === 6 || nid === 7) return 'CS'
  if (nid >= 0x7880 && nid <= 0x7fef) return '地上'
  return 'その他'
}

/** service_type values worth showing in the guide: digital TV (1) and 4K (0xAD). Null is unknown/legacy, kept visible. */
function isGuideServiceType(serviceType: unknown): boolean {
  if (serviceType === null || serviceType === undefined) return true
  const value = Number(serviceType)
  return value === 1 || value === 0xad
}

function genreLevel1(genre: number | null | undefined): number | null {
  if (genre === null || genre === undefined) return null
  return (genre >> 4) & 0xf
}

function genreLabel(genre: number | null | undefined): string {
  const level1 = genreLevel1(genre)
  if (level1 === null) return '—'
  return GENRE_LABELS[level1] ?? `不明(0x${level1.toString(16)})`
}

function genreColor(genre: number | null | undefined): string {
  const level1 = genreLevel1(genre)
  if (level1 === null) return 'transparent'
  return GENRE_COLORS[level1] ?? 'var(--muted)'
}

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

/** One (nid, tsid) multiplex: a main service (lowest sid) plus its sub-channels (sid asc). */
type ServiceGroup = {
  key: string
  main: Service
  subs: Service[]
}

/** A group as actually rendered: sub-channels whose EPG is identical to (or empty vs.) the main are dropped. */
type RenderGroup = {
  key: string
  band: BandCategory
  columns: Service[]
  /** program slot keys ((start_at, duration_secs, name)) shared by every column; only meaningful when columns.length > 1. */
  sharedKeys: Set<string>
  startIndex: number
}

type DisplayColumn = {
  service: Service
  groupKey: string
  sharedKeys: Set<string>
}

/** Comparator for services within the same band: 地上 sorts by remote_control_key (nulls last), everything else by sid. */
function compareServices(a: Service, b: Service): number {
  if (a.band === '地上') {
    const ra = a.remoteControlKey ?? Number.POSITIVE_INFINITY
    const rb = b.remoteControlKey ?? Number.POSITIVE_INFINITY
    if (ra !== rb) return ra - rb
  }
  return a.sid - b.sid
}

function compareGroups(a: RenderGroup, b: RenderGroup): number {
  if (BAND_ORDER[a.band] !== BAND_ORDER[b.band]) return BAND_ORDER[a.band] - BAND_ORDER[b.band]
  return compareServices(a.columns[0], b.columns[0])
}

function groupServices(services: Service[]): ServiceGroup[] {
  const map = new Map<string, Service[]>()
  for (const svc of services) {
    const key = `${svc.nid}:${svc.tsid}`
    const list = map.get(key)
    if (list) list.push(svc)
    else map.set(key, [svc])
  }
  const groups: ServiceGroup[] = []
  for (const [key, list] of map) {
    const sorted = [...list].sort((a, b) => a.sid - b.sid)
    groups.push({ key, main: sorted[0], subs: sorted.slice(1) })
  }
  return groups
}

/** Slot identity used both for "is this sub-channel just a repeat of the main?" and cross-column cell merging. */
function programSlotKey(program: Program): string {
  return `${program.start_at}:${program.duration_secs}:${program.name}`
}

function programSlotKeySet(programs: Program[]): Set<string> {
  return new Set(programs.map(programSlotKey))
}

function sameSlotSets(a: Set<string>, b: Set<string>): boolean {
  if (a.size !== b.size) return false
  for (const key of a) if (!b.has(key)) return false
  return true
}

// --- date helpers ------------------------------------------------------

function fmtDateInput(d: Date): string {
  const y = d.getFullYear()
  const m = String(d.getMonth() + 1).padStart(2, '0')
  const day = String(d.getDate()).padStart(2, '0')
  return `${y}-${m}-${day}`
}

function fmtTime(epochSec: number): string {
  const d = new Date(epochSec * 1000)
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}

// --- state ---------------------------------------------------------

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
let clockTimer = 0

function shiftDate(days: number) {
  const [y, m, d] = selectedDate.value.split('-').map(Number)
  const dt = new Date(y, m - 1, d)
  dt.setDate(dt.getDate() + days)
  selectedDate.value = fmtDateInput(dt)
}

function goToday() {
  selectedDate.value = fmtDateInput(new Date())
}

const gridStart = computed(() => {
  const [y, m, d] = selectedDate.value.split('-').map(Number)
  return new Date(y, m - 1, d, GRID_START_HOUR, 0, 0, 0)
})

const gridBounds = computed(() => {
  const since = Math.floor(gridStart.value.getTime() / 1000)
  return { since, until: since + TOTAL_MINUTES * 60 }
})

const isToday = computed(() => selectedDate.value === fmtDateInput(new Date()))

const nowOffset = computed(() => {
  const minutes = (now.value / 1000 - gridBounds.value.since) / 60
  return minutes * PX_PER_MIN
})

const showNowLine = computed(
  () => isToday.value && nowOffset.value >= 0 && nowOffset.value <= TOTAL_HEIGHT,
)

const hourMarks = computed(() =>
  Array.from({ length: 24 }, (_, i) => {
    const hour = GRID_START_HOUR + i
    return {
      offset: i * 60 * PX_PER_MIN,
      label: `${String(hour).padStart(2, '0')}:00`,
    }
  }),
)

// --- data loading ----------------------------------------------------

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
}

// --- derived data ------------------------------------------------------

const services = computed<Service[]>(() => {
  const seen = new Map<string, Service>()
  for (const row of rawChannels.value) {
    const nid = Number(row.nid)
    const sid = Number(row.sid)
    const tsid = Number(row.tsid)
    if (!Number.isFinite(nid) || !Number.isFinite(sid) || !Number.isFinite(tsid)) continue
    if (!isGuideServiceType(row.service_type)) continue
    const key = `${nid}:${sid}`
    if (seen.has(key)) continue
    const remoteControlKey =
      row.remote_control_key === null || row.remote_control_key === undefined
        ? null
        : Number(row.remote_control_key)
    seen.set(key, {
      key,
      nid,
      sid,
      tsid,
      name: String(row.channel_name ?? `${nid}-${sid}`),
      band: bandCategory(row.band_type, nid),
      remoteControlKey:
        remoteControlKey !== null && Number.isFinite(remoteControlKey) ? remoteControlKey : null,
      region:
        row.terrestrial_region === null || row.terrestrial_region === undefined
          ? null
          : String(row.terrestrial_region),
    })
  }
  return Array.from(seen.values()).sort((a, b) => a.nid - b.nid || a.sid - b.sid)
})

/** Region choices offered once "地上" is selected, in first-seen order. */
const regionOptions = computed(() => {
  const seen: string[] = []
  for (const svc of services.value) {
    if (svc.band !== '地上' || svc.region === null) continue
    if (!seen.includes(svc.region)) seen.push(svc.region)
  }
  return seen
})

watch(bandFilter, (value) => {
  if (value !== '地上') regionFilter.value = 'すべて'
})

const filteredServices = computed(() => {
  const query = serviceQuery.value.trim().toLowerCase()
  return services.value.filter((svc) => {
    if (bandFilter.value !== 'すべて' && svc.band !== bandFilter.value) return false
    if (
      bandFilter.value === '地上' &&
      regionFilter.value !== 'すべて' &&
      svc.region !== regionFilter.value
    )
      return false
    if (!query) return true
    return (
      svc.name.toLowerCase().includes(query) ||
      String(svc.nid).includes(query) ||
      String(svc.sid).includes(query)
    )
  })
})

const programsByService = computed(() => {
  const map = new Map<string, Program[]>()
  for (const row of rawPrograms.value) {
    const nid = Number(row.nid)
    const sid = Number(row.sid)
    const start_at = Number(row.start_at)
    const duration_secs = Number(row.duration_secs)
    if (![nid, sid, start_at, duration_secs].every(Number.isFinite)) continue
    const key = `${nid}:${sid}`
    const program: Program = {
      id: Number(row.id),
      nid,
      sid,
      event_id: Number(row.event_id),
      start_at,
      duration_secs,
      name: String(row.name ?? '(番組名不明)'),
      description: String(row.description ?? ''),
      extended: String(row.extended ?? ''),
      genre: row.genre === null || row.genre === undefined ? null : Number(row.genre),
    }
    const list = map.get(key)
    if (list) list.push(program)
    else map.set(key, [program])
  }
  for (const list of map.values()) list.sort((a, b) => a.start_at - b.start_at)
  return map
})

const hasAnyPrograms = computed(() => rawPrograms.value.length > 0)

function programsFor(svc: Service): Program[] {
  return programsByService.value.get(svc.key) ?? []
}

/**
 * (nid, tsid) multiplexes collapsed to what's actually shown: a sub-channel is dropped when its
 * programme list for the selected day is empty or an exact match of the main service's, and kept
 * (as its own column, immediately right of the main) otherwise. `sharedKeys` holds the slot keys
 * that are identical across every kept column of the group, used to merge those cells in the grid.
 */
const renderGroups = computed<RenderGroup[]>(() => {
  const groups = groupServices(filteredServices.value).map((group): RenderGroup => {
    const mainKeys = programSlotKeySet(programsFor(group.main))
    const visibleSubs = group.subs.filter((sub) => {
      const subPrograms = programsFor(sub)
      if (subPrograms.length === 0) return false
      return !sameSlotSets(mainKeys, programSlotKeySet(subPrograms))
    })
    const columns = [group.main, ...visibleSubs]
    let sharedKeys = new Set<string>()
    if (columns.length > 1) {
      sharedKeys = new Set(mainKeys)
      for (const sub of visibleSubs) {
        const subKeys = programSlotKeySet(programsFor(sub))
        for (const key of sharedKeys) if (!subKeys.has(key)) sharedKeys.delete(key)
      }
    }
    return { key: group.key, band: group.main.band, columns, sharedKeys, startIndex: 0 }
  })
  groups.sort(compareGroups)
  let offset = 0
  for (const group of groups) {
    group.startIndex = offset
    offset += group.columns.length
  }
  return groups
})

/** Flattened one-entry-per-grid-column view of renderGroups, in display order. */
const displayColumns = computed<DisplayColumn[]>(() =>
  renderGroups.value.flatMap((group) =>
    group.columns.map((service) => ({
      service,
      groupKey: group.key,
      sharedKeys: group.sharedKeys,
    })),
  ),
)

/** Groups whose sub-channels share at least one program slot with the main; these get a spanning cell overlay. */
const spanGroups = computed(() =>
  renderGroups.value.filter((g) => g.columns.length > 1 && g.sharedKeys.size > 0),
)

/** Programs to draw in an individual column: visible in the day grid and not already covered by a spanning cell. */
function columnPrograms(col: DisplayColumn): Program[] {
  return programsFor(col.service)
    .filter(visible)
    .filter((program) => !col.sharedKeys.has(programSlotKey(program)))
}

/** The (deduplicated) merged programs to draw once, spanning every column of the group. */
function spanPrograms(group: RenderGroup): Program[] {
  return programsFor(group.columns[0])
    .filter(visible)
    .filter((program) => group.sharedKeys.has(programSlotKey(program)))
}

function spanOverlayStyle(group: RenderGroup) {
  return {
    gridColumn: `${group.startIndex + 2} / span ${group.columns.length}`,
    gridRow: '2 / 3',
    height: `${TOTAL_HEIGHT}px`,
  }
}

function cellStyle(program: Program) {
  const { since } = gridBounds.value
  const topMin = Math.max(0, (program.start_at - since) / 60)
  const bottomMin = Math.min(TOTAL_MINUTES, (program.start_at + program.duration_secs - since) / 60)
  const height = Math.max(bottomMin - topMin, 1) * PX_PER_MIN
  return {
    top: `${topMin * PX_PER_MIN}px`,
    height: `${height}px`,
    borderLeftColor: genreColor(program.genre),
    background: `color-mix(in srgb, ${genreColor(program.genre)} 12%, var(--surface))`,
  }
}

function visible(program: Program): boolean {
  const { since, until } = gridBounds.value
  return program.start_at < until && program.start_at + program.duration_secs > since
}

function openDetail(program: Program) {
  detail.value = program
}

function closeDetail() {
  detail.value = null
}

const previewProgram = ref<Program | null>(null)
function openPreview(program: Program) {
  detail.value = null
  previewProgram.value = program
}
function closePreview() {
  previewProgram.value = null
}

function gridTemplateColumns() {
  return `64px repeat(${displayColumns.value.length}, minmax(160px, 1fr))`
}

watch(selectedDate, () => void loadPrograms())

function onKeydown(event: KeyboardEvent) {
  if (event.key !== 'Escape') return
  if (previewProgram.value) closePreview()
  else closeDetail()
}

onMounted(() => {
  void refresh()
  clockTimer = window.setInterval(() => (now.value = Date.now()), 30000)
  window.addEventListener('keydown', onKeydown)
})

onUnmounted(() => {
  window.clearInterval(clockTimer)
  window.removeEventListener('keydown', onKeydown)
})
</script>

<template>
  <section class="view">
    <div class="view-heading">
      <div>
        <h2>番組表</h2>
        <p>視聴中チューナーが収集したEPGデータを表示します（歯抜けは正常です）</p>
      </div>
      <button class="button secondary" @click="refresh" v-text="loading ? '更新中…' : '更新'" />
    </div>

    <div class="guide-toolbar">
      <div class="guide-date-nav">
        <button class="button small secondary" aria-label="前日" @click="shiftDate(-1)">◀</button>
        <button class="button small secondary" @click="goToday">今日</button>
        <button class="button small secondary" aria-label="翌日" @click="shiftDate(1)">▶</button>
        <input v-model="selectedDate" type="date" aria-label="日付を選択" />
      </div>
      <label class="field guide-band-filter">
        <span>放送種別</span>
        <select v-model="bandFilter">
          <option value="すべて">すべて</option>
          <option value="地上">地上波</option>
          <option value="BS">BS</option>
          <option value="CS">CS</option>
        </select>
      </label>
      <label v-if="bandFilter === '地上'" class="field guide-region-filter">
        <span>地域</span>
        <select v-model="regionFilter">
          <option value="すべて">すべての地域</option>
          <option v-for="region in regionOptions" :key="region" :value="region" v-text="region" />
        </select>
      </label>
      <label class="search guide-service-search">
        <span>サービス絞り込み</span>
        <input v-model="serviceQuery" type="search" placeholder="チャンネル名、NID、SID" />
      </label>
    </div>

    <p v-if="error" class="notice error" role="alert" v-text="error" />

    <p v-if="!hasAnyPrograms && !loading" class="empty-state">
      番組情報がありません。番組情報は視聴中のチャンネルから自動収集されます。
    </p>

    <div v-else class="guide-scroll">
      <!--
        Every cell below is explicitly placed via gridColumn/gridRow rather than relying on
        grid-auto-flow. The span overlays (merged sub-channel cells) need explicit both-axis
        placement to cover multiple columns, and CSS Grid places all explicitly-both-axis items
        *before* auto-placed ones regardless of DOM order — mixing that with auto-placed .guide-col
        cells would make the auto items skip the overlay's reserved cells and drift out of column.
        Placing everything explicitly sidesteps that entirely.
      -->
      <div class="guide-grid" :style="{ gridTemplateColumns: gridTemplateColumns() }">
        <div class="guide-corner" aria-hidden="true" :style="{ gridColumn: 1, gridRow: 1 }" />
        <div
          v-for="(col, index) in displayColumns"
          :key="`h-${col.service.key}`"
          class="guide-header-cell"
          :style="{ gridColumn: index + 2, gridRow: 1 }"
          v-text="col.service.name"
        />

        <div
          class="guide-timeaxis"
          :style="{ gridColumn: 1, gridRow: 2, height: `${TOTAL_HEIGHT}px` }"
        >
          <div
            v-for="mark in hourMarks"
            :key="mark.label"
            class="guide-hour-label"
            :style="{ top: `${mark.offset}px` }"
            v-text="mark.label"
          />
        </div>

        <div
          v-for="(col, index) in displayColumns"
          :key="`c-${col.service.key}`"
          class="guide-col"
          :style="{ gridColumn: index + 2, gridRow: 2, height: `${TOTAL_HEIGHT}px` }"
        >
          <div v-if="showNowLine" class="guide-now-line" :style="{ top: `${nowOffset}px` }" />
          <button
            v-for="program in columnPrograms(col)"
            :key="program.id"
            type="button"
            class="guide-cell"
            :style="cellStyle(program)"
            @click="openDetail(program)"
          >
            <strong v-text="program.name" />
            <span class="guide-cell-time" v-text="fmtTime(program.start_at)" />
          </button>
        </div>

        <!-- Multi-programme (sub-channel) groups: cells identical across every column of the
             group are drawn once here, spanning the whole group, on top of the per-column cells
             above (which already skip these slots via DisplayColumn.sharedKeys). -->
        <div
          v-for="group in spanGroups"
          :key="`span-${group.key}`"
          class="guide-col guide-span-overlay"
          :style="spanOverlayStyle(group)"
        >
          <button
            v-for="program in spanPrograms(group)"
            :key="program.id"
            type="button"
            class="guide-cell"
            :style="cellStyle(program)"
            @click="openDetail(program)"
          >
            <strong v-text="program.name" />
            <span class="guide-cell-time" v-text="fmtTime(program.start_at)" />
          </button>
        </div>
      </div>
      <p v-if="!displayColumns.length" class="empty-state">条件に一致するサービスがありません</p>
    </div>

    <div v-if="detail" class="dialog-backdrop" @click.self="closeDetail">
      <section
        class="dialog guide-detail-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="guide-detail-title"
      >
        <h2 id="guide-detail-title" v-text="detail.name" />
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
          <button class="button" @click="openPreview(detail)">▶ プレビュー</button>
          <button class="button secondary" @click="closeDetail">閉じる</button>
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
              v-text="`${previewProgram.name || '—'}（SID ${previewProgram.sid}）`"
            />
          </div>
          <button class="button secondary" @click="closePreview">閉じる</button>
        </div>
        <PreviewPlayer :key="previewProgram.sid" :initial-sid="previewProgram.sid" />
      </section>
    </div>
  </section>
</template>

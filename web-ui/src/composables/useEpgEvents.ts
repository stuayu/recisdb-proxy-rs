import { onUnmounted } from 'vue'
import type { JsonRecord } from '../api'

export type EpgProgramEvent = {
  type?: unknown
  program?: JsonRecord
}

export type EpgScanState = {
  network_id: number
  tsid: number
  last_scan_started_at: number | null
  last_scan_completed_at: number | null
  last_eit_received_at: number | null
  section_coverage_until: number | null
  services_total: number | null
  services_complete: number | null
  last_complete_at: number | null
  last_scan_status: string | null
  last_failure_reason: string | null
}

export type EpgStatusKind = 'scanning' | 'complete' | 'partial' | 'stale' | 'none' | 'failed'

export type EpgStatusPresentation = {
  kind: EpgStatusKind
  label: string
  text: string
}

export function programEventKey(row: JsonRecord): string {
  return `${Number(row.nid)}:${Number(row.tsid)}:${Number(row.sid)}:${Number(row.event_id)}`
}

export function normalizeProgramRow(row: JsonRecord): JsonRecord {
  return { ...row, key: programEventKey(row) }
}

export function buildProgramIndex(rows: JsonRecord[]): Map<string, number> {
  const index = new Map<string, number>()
  rows.forEach((row, position) => index.set(programEventKey(row), position))
  return index
}

/** Map の索引で1イベントを処理する。戻り値は配列変更の有無。 */
export function mergeProgramEvent(
  rows: JsonRecord[],
  index: Map<string, number>,
  event: EpgProgramEvent,
  loadedWindows: Array<[number, number]>,
): boolean {
  const type = event.type
  if (type !== 'create' && type !== 'update' && type !== 'delete') return false
  const program = event.program
  if (!program) return false
  const key = programEventKey(program)
  const existing = index.get(key)
  const startAt = Number(program.start_at)
  const inWindow = loadedWindows.some(([since, until]) => startAt >= since && startAt < until)
  if (!inWindow && existing === undefined) return false

  if (type === 'delete') {
    if (existing === undefined) return false
    rows.splice(existing, 1)
    index.clear()
    rows.forEach((row, position) => index.set(programEventKey(row), position))
    return true
  }

  const next = normalizeProgramRow(program)
  if (existing === undefined) {
    index.set(key, rows.length)
    rows.push(next)
  } else {
    // SSE に id は無い。既存行の id/詳細取得状態を保持してフィールドだけ更新。
    rows[existing] = { ...rows[existing], ...next, id: rows[existing].id, loaded: rows[existing].loaded }
  }
  return true
}

function ageText(timestamp: number | null, now: number): string {
  if (timestamp === null || !Number.isFinite(timestamp)) return '更新時刻不明'
  const seconds = Math.max(0, now - timestamp)
  if (seconds < 60) return '更新 1分未満前'
  if (seconds < 3600) return `更新 ${Math.floor(seconds / 60)}分前`
  return `更新 ${Math.floor(seconds / 3600)}時間前`
}

export function epgStatusPresentation(
  state: EpgScanState | undefined,
  now: number,
  targetFutureCoverageHours = 168,
  targetRefreshSecs = 86400,
): EpgStatusPresentation {
  if (!state) return { kind: 'none', label: '未取得', text: 'EPG未取得' }
  const scanning = state.last_scan_started_at !== null &&
    (state.last_scan_completed_at === null || state.last_scan_started_at > state.last_scan_completed_at)
  const failed = ['failed', 'no_data', 'cpu_aborted', 'preempted'].includes(state.last_scan_status ?? '')
  const coverage = state.section_coverage_until
  const covered = coverage !== null && coverage >= now + targetFutureCoverageHours * 3600
  const servicesPartial = state.services_total !== null && state.services_complete !== null &&
    state.services_complete < state.services_total
  const fresh = state.last_complete_at !== null && state.last_complete_at + targetRefreshSecs > now
  const days = coverage === null ? 0 : Math.max(0, Math.floor((coverage - now) / 86400))
  const coverageText = days >= 1 ? `${days}日分` : '1日未満'

  if (scanning) return { kind: 'scanning', label: '取得中', text: `${coverageText} / EPG取得中…` }
  if (failed) return { kind: 'failed', label: '失敗', text: `EPG取得失敗${state.last_failure_reason ? `: ${state.last_failure_reason}` : ''}` }
  if (coverage === null) return { kind: 'none', label: '未取得', text: 'EPG未取得' }
  if (servicesPartial || !covered) return { kind: 'partial', label: '一部取得', text: `${coverageText}のみ` }
  if (!fresh) return { kind: 'stale', label: '古い', text: `${coverageText} / ${ageText(state.last_complete_at, now)}` }
  return { kind: 'complete', label: '取得済み', text: `${coverageText}取得済み / ${ageText(state.last_complete_at, now)}` }
}

/**
 * 「接続できた」だけではバックオフを戻さない。SSE を通さないリバースプロキシや、
 * `/api/epg/events` に非ストリームの 200 を返す構成に当たると、
 * 接続直後に本文が終端して再接続 → onReconnect で全件再取得、が毎秒繰り返される。
 * 2万件の再取得と再描画が走り続けて番組表の選択が飛ぶ。
 * 少なくともこの時間だけ繋がり続けたか、1フレームでも受け取れたときにだけ
 * 「安定した接続」とみなして待ち時間を初期値へ戻す。
 */
const STABLE_CONNECTION_MS = 5000

/**
 * 接続をバックオフのリセットに値する「安定した接続」と数えてよいか。
 * 純関数にして、切断ループでバックオフが戻らないことをテストできるようにする。
 */
export function isStableConnection(startedAt: number, endedAt: number, receivedFrame: boolean): boolean {
  return receivedFrame || endedAt - startedAt >= STABLE_CONNECTION_MS
}

type EpgEventsOptions = {
  onProgram: (event: EpgProgramEvent) => void
  onLagged: () => void
  onStatus: (states: EpgScanState[]) => void
  onReconnect: () => void
}

function parseSseFrame(frame: string, options: EpgEventsOptions): void {
  let eventName = 'message'
  const data: string[] = []
  for (const line of frame.split('\n')) {
    if (line.startsWith('event:')) eventName = line.slice(6).trim()
    else if (line.startsWith('data:')) data.push(line.slice(5).trimStart())
  }
  if (eventName === 'ping' || data.length === 0) return
  try {
    const payload = JSON.parse(data.join('\n')) as JsonRecord
    if (eventName === 'program') options.onProgram(payload as EpgProgramEvent)
    else if (eventName === 'lagged') options.onLagged()
    else if (eventName === 'epg_status') {
      const states = payload.states
      if (Array.isArray(states)) options.onStatus(states as EpgScanState[])
    }
  } catch {
    // 不正な1フレームで接続全体を落とさない。
  }
}

export function useEpgEvents(options: EpgEventsOptions) {
  let controller: AbortController | null = null
  let retryTimer: number | null = null
  let retryDelay = 1000
  let connected = false
  let stopped = false

  const clearRetry = () => {
    if (retryTimer !== null) window.clearTimeout(retryTimer)
    retryTimer = null
  }
  const schedule = () => {
    if (stopped || document.visibilityState === 'hidden' || retryTimer !== null) return
    retryTimer = window.setTimeout(() => {
      retryTimer = null
      void connect()
    }, retryDelay)
    retryDelay = Math.min(30000, retryDelay * 2)
  }
  const connect = async () => {
    if (stopped || document.visibilityState === 'hidden') return
    controller?.abort()
    const currentController = new AbortController()
    controller = currentController
    const headers = new Headers({ Accept: 'text/event-stream' })
    const token = localStorage.getItem('recisdbApiToken')
    if (token) headers.set('Authorization', `Bearer ${token}`)
    const startedAt = Date.now()
    let receivedFrame = false
    try {
      const response = await fetch('/api/epg/events', { headers, signal: currentController.signal })
      if (!response.ok || !response.body) throw new Error(`SSE ${response.status}`)
      // ストリームでない 200 を「接続成功」と数えない。
      const contentType = response.headers.get('Content-Type') ?? ''
      if (!contentType.includes('text/event-stream')) throw new Error(`SSE content-type ${contentType}`)
      const wasConnected = connected
      connected = true
      if (wasConnected) options.onReconnect()
      const reader = response.body.getReader()
      const decoder = new TextDecoder()
      let buffer = ''
      while (!currentController.signal.aborted) {
        const result = await reader.read()
        if (result.done) break
        receivedFrame = true
        buffer += decoder.decode(result.value, { stream: true })
        const frames = buffer.split('\n\n')
        buffer = frames.pop() ?? ''
        frames.forEach((frame) => parseSseFrame(frame, options))
      }
    } catch {
      // abort と一時的な切断は同じ再接続経路へ送る。
    } finally {
      if (isStableConnection(startedAt, Date.now(), receivedFrame)) retryDelay = 1000
      if (!stopped && !currentController.signal.aborted) schedule()
    }
  }
  const onVisibility = () => {
    if (document.visibilityState === 'visible' && !controller?.signal.aborted) {
      clearRetry()
      retryDelay = 1000
      void connect()
    }
  }
  const start = () => {
    stopped = false
    document.addEventListener('visibilitychange', onVisibility)
    void connect()
  }
  const stop = () => {
    stopped = true
    clearRetry()
    controller?.abort()
    controller = null
    document.removeEventListener('visibilitychange', onVisibility)
  }
  onUnmounted(stop)
  return { start, stop }
}

import assert from 'node:assert/strict'
import test from 'node:test'
import {
  buildProgramIndex,
  epgStatusPresentation,
  isStableConnection,
  mergeProgramEvent,
  programEventKey,
  type EpgScanState,
} from './useEpgEvents.ts'

const WINDOW: Array<[number, number]> = [[1000, 2000]]

function row(eventId: number, extra: Record<string, unknown> = {}) {
  return { nid: 1, tsid: 2, sid: 3, event_id: eventId, start_at: 1500, duration_secs: 60, ...extra }
}

test('update keeps the stored id and lazily loaded detail', () => {
  const rows = [row(10, { id: 77, loaded: true, name: '旧' })]
  const index = buildProgramIndex(rows)
  assert.equal(mergeProgramEvent(rows, index, { type: 'update', program: row(10, { name: '新' }) }, WINDOW), true)
  assert.equal(rows[0].name, '新')
  // SSE に id は来ない。消すと番組詳細の遅延取得が壊れる。
  assert.equal(rows[0].id, 77)
  assert.equal(rows[0].loaded, true)
})

test('create appends a new row and delete removes it', () => {
  const rows = [row(10)]
  const index = buildProgramIndex(rows)
  assert.equal(mergeProgramEvent(rows, index, { type: 'create', program: row(11) }, WINDOW), true)
  assert.equal(rows.length, 2)
  assert.equal(mergeProgramEvent(rows, index, { type: 'delete', program: row(10) }, WINDOW), true)
  assert.equal(rows.length, 1)
  assert.equal(index.get(programEventKey(row(11))), 0)
})

test('unknown event types and programs outside the loaded window are ignored', () => {
  const rows = [row(10)]
  const index = buildProgramIndex(rows)
  assert.equal(mergeProgramEvent(rows, index, { type: 'rename', program: row(12) }, WINDOW), false)
  assert.equal(mergeProgramEvent(rows, index, { type: 'create', program: row(12, { start_at: 99999 }) }, WINDOW), false)
  assert.equal(rows.length, 1)
})

test('an already loaded program outside the window is still updated', () => {
  // 取得済みの行は、時間窓の外へ動いても更新を反映する (延長・繰り下げ)。
  const rows = [row(10, { start_at: 1500 })]
  const index = buildProgramIndex(rows)
  assert.equal(mergeProgramEvent(rows, index, { type: 'update', program: row(10, { start_at: 99999 }) }, WINDOW), true)
  assert.equal(rows[0].start_at, 99999)
})

test('merging many events touches each row through the index, not a full scan', () => {
  const rows = Array.from({ length: 2000 }, (_, i) => row(i))
  const index = buildProgramIndex(rows)
  for (let i = 0; i < 2000; i += 1) {
    mergeProgramEvent(rows, index, { type: 'update', program: row(i, { name: `n${i}` }) }, WINDOW)
  }
  assert.equal(rows.length, 2000)
  assert.equal(rows[1999].name, 'n1999')
})

const NOW = 1_700_000_000

function state(extra: Partial<EpgScanState> = {}): EpgScanState {
  return {
    network_id: 1,
    tsid: 2,
    last_scan_started_at: NOW - 7200,
    last_scan_completed_at: NOW - 3600,
    last_eit_received_at: NOW - 3600,
    section_coverage_until: NOW + 168 * 3600,
    services_total: 4,
    services_complete: 4,
    last_complete_at: NOW - 3600,
    last_scan_status: 'complete',
    last_failure_reason: null,
    ...extra,
  }
}

test('a scan started after the last completion reads as scanning', () => {
  assert.equal(epgStatusPresentation(state({ last_scan_started_at: NOW - 60 }), NOW).kind, 'scanning')
})

test('enough coverage that has gone stale is not reported as complete', () => {
  assert.equal(epgStatusPresentation(state({ last_complete_at: NOW - 200000 }), NOW).kind, 'stale')
})

test('a null section coverage reads as not collected', () => {
  assert.equal(epgStatusPresentation(state({ section_coverage_until: null }), NOW).kind, 'none')
})

test('an incomplete service count reads as partial even when coverage looks sufficient', () => {
  assert.equal(epgStatusPresentation(state({ services_complete: 1 }), NOW).kind, 'partial')
})

test('a missing state reads as not collected rather than complete', () => {
  assert.equal(epgStatusPresentation(undefined, NOW).kind, 'none')
})

test('a connection that dies immediately does not reset the backoff', () => {
  // SSE を通さないプロキシや非ストリームの 200 に当たったとき、
  // 毎秒の再接続と全件再取得を繰り返さないための回帰テスト。
  assert.equal(isStableConnection(0, 30, false), false)
  assert.equal(isStableConnection(0, 30, true), true)
  assert.equal(isStableConnection(0, 5000, false), true)
})

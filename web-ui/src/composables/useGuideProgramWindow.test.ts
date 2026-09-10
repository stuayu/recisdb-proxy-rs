import assert from 'node:assert/strict'
import test from 'node:test'
import { calculateGuideProgramWindow } from './useGuideProgramWindow.ts'

const GRID_SINCE = 6 * 60 * 60
const GRID_UNTIL = GRID_SINCE + 24 * 60 * 60
const PX_PER_MIN = 1
const STEP = 4 * 60 * 60

test('12:00-16:00 requests a window containing the visible time', () => {
  const result = calculateGuideProgramWindow({
    top: 6 * 60,
    bottom: 10 * 60,
    gridSince: GRID_SINCE,
    gridUntil: GRID_UNTIL,
    pxPerMin: PX_PER_MIN,
    bufferPx: 60,
    stepSecs: STEP,
  })

  assert.deepEqual(result, [10 * 60 * 60, 18 * 60 * 60])
})

test('clamps the requested window to both grid bounds', () => {
  const result = calculateGuideProgramWindow({
    top: -100,
    bottom: 24 * 60,
    gridSince: GRID_SINCE,
    gridUntil: GRID_UNTIL,
    pxPerMin: PX_PER_MIN,
    bufferPx: 3 * 60,
    stepSecs: STEP,
  })

  assert.deepEqual(result, [GRID_SINCE, GRID_UNTIL])
})

test('the same scroll position produces the same snapped window', () => {
  const input = {
    top: 7 * 60 + 13,
    bottom: 11 * 60 + 29,
    gridSince: GRID_SINCE,
    gridUntil: GRID_UNTIL,
    pxPerMin: PX_PER_MIN,
    bufferPx: 60,
    stepSecs: STEP,
  }

  assert.deepEqual(calculateGuideProgramWindow(input), calculateGuideProgramWindow(input))
})

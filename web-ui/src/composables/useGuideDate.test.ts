import assert from 'node:assert/strict'
import test from 'node:test'
import { broadcastDateInput } from './useGuideDate.ts'

const GRID_START_HOUR = 6

function localTime(year: number, month: number, day: number, hour: number, minute: number): number {
  return new Date(year, month - 1, day, hour, minute).getTime()
}

test('02:03 belongs to the previous broadcast date', () => {
  assert.equal(broadcastDateInput(localTime(2026, 9, 11, 2, 3), GRID_START_HOUR), '2026-09-10')
})

test('06:00 starts the current broadcast date', () => {
  assert.equal(broadcastDateInput(localTime(2026, 9, 11, 6, 0), GRID_START_HOUR), '2026-09-11')
})

test('05:59 belongs to the previous broadcast date', () => {
  assert.equal(broadcastDateInput(localTime(2026, 9, 11, 5, 59), GRID_START_HOUR), '2026-09-10')
})

test('23:30 belongs to the current broadcast date', () => {
  assert.equal(broadcastDateInput(localTime(2026, 9, 11, 23, 30), GRID_START_HOUR), '2026-09-11')
})

test('crossing into a leap-year February returns February 29', () => {
  assert.equal(broadcastDateInput(localTime(2024, 3, 1, 2, 0), GRID_START_HOUR), '2024-02-29')
})

test('crossing into a non-leap-year February returns February 28', () => {
  assert.equal(broadcastDateInput(localTime(2023, 3, 1, 2, 0), GRID_START_HOUR), '2023-02-28')
})

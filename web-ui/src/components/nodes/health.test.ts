import assert from 'node:assert/strict'
import test from 'node:test'
import { healthLabel } from './health.ts'

test('converts probe health to Japanese status', () => {
  assert.deepEqual(healthLabel(undefined), ['bad', '利用不可'])
  assert.deepEqual(healthLabel({ health: { state: 'degraded', rtt_p95_ms: 1, stall_rate: 0 } } as never), [
    'warn',
    '不安定',
  ])
})

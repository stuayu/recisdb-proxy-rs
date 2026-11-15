import assert from 'node:assert/strict'
import test from 'node:test'

import { boncaslinkNetworkReaderPayload } from './boncaslinkSettings.ts'

test('serializes a network reader NID as hexadecimal text for the API', () => {
  assert.deepEqual(
    boncaslinkNetworkReaderPayload([
      { nid: 'FFF7', reader: 'stuayu-mate01:6900', custom: true },
    ]),
    [{ nid: 'FFF7', reader: 'boncaslink://stuayu-mate01:6900' }],
  )
})

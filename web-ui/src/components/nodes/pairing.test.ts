import assert from 'node:assert/strict'
import test from 'node:test'
import QRCode from 'qrcode'
import { isPairingExpired, pairingConnectionText, parsePairingConnection } from './pairing.ts'

test('builds an offline-safe pairing connection for QR and copy', () => {
  const text = pairingConnectionText('http://100.64.0.2:20773', 'ABCD-EFGH')
  assert.equal(text, 'recisdb://pair?endpoint=http%3A%2F%2F100.64.0.2%3A20773&code=ABCD-EFGH')
  assert.deepEqual(parsePairingConnection(text), {
    base_url: 'http://100.64.0.2:20773',
    code: 'ABCD-EFGH',
  })
})

test('encodes pairing text as QR data and detects expiry', async () => {
  const svg = await QRCode.toString(pairingConnectionText('https://node.example', 'ABCD-EFGH'), {
    type: 'svg',
  })
  assert.match(svg, /^<svg/)
  assert.equal(isPairingExpired(1_000, 1_000), true)
  assert.equal(isPairingExpired(1_001, 1_000), false)
})

test('parses recisdb pairing URL', () => {
  assert.deepEqual(
    parsePairingConnection('recisdb://pair?endpoint=http%3A%2F%2F100.64.0.2%3A20773&code=ABCD-EFGH'),
    { base_url: 'http://100.64.0.2:20773', code: 'ABCD-EFGH' },
  )
})

test('rejects pairing URL without endpoint', () => {
  assert.throws(() => parsePairingConnection('recisdb://pair?code=ABCD-EFGH'), /接続先がありません/)
})

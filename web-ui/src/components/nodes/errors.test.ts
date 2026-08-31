import assert from 'node:assert/strict'
import test from 'node:test'
import { nodeErrorMessage } from './errorMessages.ts'

test('translates machine-readable node errors', () => {
  assert.match(nodeErrorMessage('node_not_paired', 'raw'), /接続設定が完了していません/)
  assert.match(nodeErrorMessage('upstream_error', 'raw'), /相手PCに接続できません/)
})

test('keeps an unknown error code actionable without exposing a blank message', () => {
  assert.equal(nodeErrorMessage('future_code', '詳細を確認してください'), '詳細を確認してください')
})

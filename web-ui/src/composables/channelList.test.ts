import assert from 'node:assert/strict'
import test from 'node:test'
import {
  buildChannelListParams,
  createLatestRequestGate,
  pageSizeChangeAction,
} from './channelList.ts'

test('page size changes reload even when the current page is already one', () => {
  assert.deepEqual(pageSizeChangeAction(1), { page: 1, reload: true })
  assert.deepEqual(pageSizeChangeAction(4), { page: 1, reload: false })
})

test('channel list query carries every active sort key and direction', () => {
  const params = buildChannelListParams({
    page: 3,
    pageSize: 50,
    query: '  NHK  ',
    sorts: [
      { key: 'channel_name', descending: false },
      { key: 'nid', descending: true },
      { key: 'sid', descending: false },
    ],
  })
  assert.equal(params.get('offset'), '100')
  assert.equal(params.get('q'), 'NHK')
  assert.deepEqual(
    ['sort', 'desc', 'sort2', 'desc2', 'sort3', 'desc3'].map((key) => params.get(key)),
    ['channel_name', 'false', 'nid', 'true', 'sid', 'false'],
  )
})

test('older channel list responses cannot become current after a newer request starts', async () => {
  const gate = createLatestRequestGate()
  let finishOld!: (value: string) => void
  const oldResponse = new Promise<string>((resolve) => {
    finishOld = resolve
  })
  const applied: string[] = []
  const load = async (response: Promise<string>) => {
    const request = gate.begin()
    const result = await response
    if (gate.isCurrent(request)) applied.push(result)
  }

  const older = load(oldResponse)
  await load(Promise.resolve('new'))
  finishOld('old')
  await older
  assert.deepEqual(applied, ['new'])
})

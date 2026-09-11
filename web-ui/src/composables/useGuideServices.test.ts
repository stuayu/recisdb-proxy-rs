import assert from 'node:assert/strict'
import test from 'node:test'
import { guideServiceGroupKey, isRealGuideService, mainGuideServices, type GuideServiceGroupFields } from './useGuideServices.ts'

type TestService = GuideServiceGroupFields & { name: string }

function service(nid: number, tsid: number, sid: number, name: string, remoteControlKey?: number | null): TestService {
  return { nid, tsid, sid, name, remoteControlKey }
}

test('SID/TSID zero rows are not guide services', () => {
  assert.equal(isRealGuideService(service(1, 1, 100, '実在')), true)
  assert.equal(isRealGuideService(service(1, 0, 100, 'TSIDなし')), false)
  assert.equal(isRealGuideService(service(1, 1, 0, 'SIDなし')), false)
  assert.equal(isRealGuideService(service(1, 0, 0, '仮行')), false)
})

test('CS services with distinct RCKs remain as eight services', () => {
  const services = [
    service(7, 28768, 294, 'ホームドラマＣＨ', 294),
    service(7, 28768, 324, 'ミュージック・エア', 324),
    service(7, 28768, 329, '歌謡ポップス', 329),
    service(7, 28768, 331, 'カートゥーン', 331),
    service(7, 28768, 340, 'ディスカバリー', 340),
    service(7, 28768, 341, 'アニマルプラネット', 341),
    service(7, 28768, 354, 'ＣＮＮｊ', 354),
    service(7, 28768, 363, '囲碁・将棋チャンネル', 363),
  ]

  assert.deepEqual(mainGuideServices(services).map(({ sid }) => sid), [294, 324, 329, 331, 340, 341, 354, 363])
})

test('BS朝日 151/152/153 is folded to SID 151', () => {
  const services = [
    service(4, 16400, 151, 'ＢＳ朝日１', 5),
    service(4, 16400, 152, 'ＢＳ朝日２', 5),
    service(4, 16400, 153, 'ＢＳ朝日３', 5),
  ]

  assert.deepEqual(mainGuideServices(services).map(({ sid }) => sid), [151])
})

test('ＮＨＫ ＢＳ 101/102 is folded to SID 101', () => {
  const services = [
    service(4, 16625, 101, 'ＮＨＫ ＢＳ', 1),
    service(4, 16625, 102, 'ＮＨＫ ＢＳ', 1),
  ]

  assert.deepEqual(mainGuideServices(services).map(({ sid }) => sid), [101])
})

test('terrestrial 17408/17409/65520 is folded to SID 17408', () => {
  const services = [
    service(32480, 32480, 17408, 'ＮＨＫ総合１・仙台', 3),
    service(32480, 32480, 17409, 'ＮＨＫ総合２・仙台', 3),
    service(32480, 32480, 65520, 'GR17', 3),
  ]

  assert.deepEqual(mainGuideServices(services).map(({ sid }) => sid), [17408])
})

test('services without RCK are never folded together', () => {
  const services = [
    service(7, 28768, 400, 'RCKなし1', null),
    service(7, 28768, 401, 'RCKなし2', undefined),
  ]

  assert.equal(guideServiceGroupKey(services[0]), '7:28768:sid:400')
  assert.equal(guideServiceGroupKey(services[1]), '7:28768:sid:401')
  assert.deepEqual(mainGuideServices(services).map(({ sid }) => sid), [400, 401])
})

export type GuideServiceGroupFields = {
  nid: number
  tsid: number
  sid: number
  remoteControlKey?: number | null
}

/** SID/TSID 0 は物理チャンネルだけの仮行。番組表のサービスではない。 */
export function isRealGuideService(service: Pick<GuideServiceGroupFields, 'sid' | 'tsid'>): boolean {
  return service.sid !== 0 && service.tsid !== 0
}

/** RCKがあるサービスは同じRCKを1局へ畳み、ないサービスはSID単位で独立させる。 */
export function guideServiceGroupKey(service: GuideServiceGroupFields): string {
  if (service.remoteControlKey === null || service.remoteControlKey === undefined) {
    return `${service.nid}:${service.tsid}:sid:${service.sid}`
  }
  return `${service.nid}:${service.tsid}:${service.remoteControlKey}`
}

/** グループごとに最小SIDのサービスだけ返す。入力順は維持する。 */
export function mainGuideServices<T extends GuideServiceGroupFields>(services: readonly T[]): T[] {
  const mains = new Map<string, T>()
  for (const service of services) {
    const key = guideServiceGroupKey(service)
    const current = mains.get(key)
    if (current === undefined || service.sid < current.sid) mains.set(key, service)
  }
  return [...mains.values()]
}

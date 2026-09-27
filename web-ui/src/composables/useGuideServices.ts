export type GuideServiceGroupFields = {
  nid: number
  tsid: number
  sid: number
  remoteControlKey?: number | null
}

export type GuideServiceForNumber = GuideServiceGroupFields & {
  band: '地上' | 'BS' | 'CS' | 'その他'
  prefectureCode?: number | null
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

/** EDCB/Komorebi-compatible display number. */
export function guideChannelNumber(service: GuideServiceForNumber, services: readonly GuideServiceForNumber[]): number {
  if (service.band === '地上') {
    if (service.remoteControlKey != null && service.remoteControlKey >= 1 && service.remoteControlKey <= 12) {
      const sameNetwork = services
        .filter((candidate) => candidate.band === '地上' && candidate.nid === service.nid)
        .sort((a, b) => a.sid - b.sid)
      return service.remoteControlKey * 10 + Math.min(8, Math.max(1, sameNetwork.findIndex((candidate) => candidate.sid === service.sid) + 1))
    }
    return service.sid % 1000
  }
  return service.sid
}

// Keep this in sync with recisdb-proxy/src/guide_services.rs::is_placeholder_program_name.
export function isPlaceholderProgramName(name: unknown): boolean {
  if (typeof name !== 'string' || name.trim() === '') return true
  return ['ご覧ください', '番組未定', '放送休止', '休止中'].some((marker) => name.trim().includes(marker))
}

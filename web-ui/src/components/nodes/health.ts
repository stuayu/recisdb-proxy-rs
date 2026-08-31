import type { ProbePath } from './types'

export function healthLabel(path: ProbePath | undefined): [string, string] {
  if (!path || path.health.state === 'unreachable') return ['bad', '利用不可']
  if (path.health.state === 'degraded') return ['warn', '不安定']
  if (path.health.rtt_p95_ms <= 30 && path.health.stall_rate < 0.02)
    return ['good', 'とても良好']
  return ['good', '良好']
}

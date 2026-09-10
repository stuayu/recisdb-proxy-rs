export type GuideProgramWindow = [number, number]

export type GuideProgramWindowInput = {
  top: number
  bottom: number
  gridSince: number
  gridUntil: number
  pxPerMin: number
  bufferPx: number
  stepSecs: number
}

/** 可視時間帯とバッファを、グリッド基準の固定幅窓へ変換する。 */
export function calculateGuideProgramWindow({
  top,
  bottom,
  gridSince,
  gridUntil,
  pxPerMin,
  bufferPx,
  stepSecs,
}: GuideProgramWindowInput): GuideProgramWindow | null {
  if (gridSince >= gridUntil || pxPerMin <= 0 || stepSecs <= 0) return null

  const gridHeightPx = (gridUntil - gridSince) / 60 * pxPerMin
  const bufferedTop = Math.min(gridHeightPx, Math.max(0, top - bufferPx))
  const bufferedBottom = Math.min(gridHeightPx, Math.max(bufferedTop, bottom + bufferPx))
  const rawSince = gridSince + Math.floor(bufferedTop / pxPerMin * 60)
  const rawUntil = gridSince + Math.ceil(bufferedBottom / pxPerMin * 60)
  const since = Math.max(
    gridSince,
    gridSince + Math.floor((rawSince - gridSince) / stepSecs) * stepSecs,
  )
  const until = Math.min(
    gridUntil,
    gridSince + Math.ceil((rawUntil - gridSince) / stepSecs) * stepSecs,
  )

  return since < until ? [since, until] : null
}

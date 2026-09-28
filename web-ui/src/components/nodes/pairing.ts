export type PairingConnection = { base_url: string; code: string }

export function pairingConnectionText(endpoint: string | string[], code: string): string {
  const selected = Array.isArray(endpoint) ? endpoint.find((value) => value.trim()) || '' : endpoint
  return `recisdb://pair?endpoint=${encodeURIComponent(selected)}&code=${encodeURIComponent(code)}`
}

export function isUsablePairingEndpoint(value: string): boolean {
  try {
    const url = new URL(value.trim())
    return (
      (url.protocol === 'http:' || url.protocol === 'https:') &&
      !['0.0.0.0', '::', '[::'].includes(url.hostname)
    )
  } catch {
    return false
  }
}

export function isPairingExpired(expiresAtUnixMs: number, currentUnixMs = Date.now()): boolean {
  return expiresAtUnixMs <= currentUnixMs
}

export function parsePairingConnection(value: string): PairingConnection {
  const raw = value.trim()
  if (!raw) throw new Error('接続情報を入力してください。')
  try {
    const url = new URL(raw)
    if (url.protocol !== 'recisdb:') throw new Error('接続情報の形式が違います。')
    const base_url = url.searchParams.get('endpoint')?.trim() || ''
    const code = url.searchParams.get('code')?.trim() || ''
    if (!/^https?:\/\//.test(base_url))
      throw new Error('接続先がありません。http://またはhttps://の接続情報を使ってください。')
    if (!code) throw new Error('ペアリングコードがありません。')
    return { base_url, code }
  } catch (cause) {
    if (cause instanceof Error && cause.message !== 'Invalid URL') throw cause
    const [base_url, code] = raw.split(/\s+/)
    if (/^https?:\/\//.test(base_url || '') && code) return { base_url, code }
    throw new Error('接続情報を読み取れません。接続情報をコピーし直してください。')
  }
}

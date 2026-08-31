export type PairingConnection = { base_url: string; code: string }

export function pairingConnectionText(endpoint: string, code: string): string {
  return `recisdb://pair?endpoint=${encodeURIComponent(endpoint)}&code=${encodeURIComponent(code)}`
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

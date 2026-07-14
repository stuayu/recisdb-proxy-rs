export type JsonRecord = Record<string, unknown>

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

function token(): string | null {
  return localStorage.getItem('recisdbApiToken')
}

function authorizedHeaders(initial?: HeadersInit): Headers {
  const headers = new Headers(initial)
  const saved = token()
  if (saved) headers.set('Authorization', `Bearer ${saved}`)
  return headers
}

export async function api<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = authorizedHeaders(init.headers)
  if (!headers.has('Accept')) headers.set('Accept', 'application/json')
  if (init.body && !(init.body instanceof FormData) && !headers.has('Content-Type')) {
    headers.set('Content-Type', 'application/json')
  }

  const response = await fetch(`/api${path}`, { ...init, headers })
  if (!response.ok) {
    const body = await response.text()
    throw new ApiError(response.status, body || `${response.status} ${response.statusText}`)
  }

  const type = response.headers.get('content-type') || ''
  return (type.includes('json') ? response.json() : response.text()) as Promise<T>
}

export async function downloadApi(path: string, fallbackName: string): Promise<void> {
  const response = await fetch(`/api${path}`, {
    headers: authorizedHeaders({ Accept: '*/*' }),
  })
  if (!response.ok) {
    const body = await response.text()
    throw new ApiError(response.status, body || `${response.status} ${response.statusText}`)
  }

  const disposition = response.headers.get('content-disposition') || ''
  const encoded = disposition.match(/filename\*=UTF-8''([^;]+)/i)?.[1]
  const plain = disposition.match(/filename="?([^";]+)"?/i)?.[1]
  const filename = encoded ? decodeURIComponent(encoded) : plain || fallbackName
  const url = URL.createObjectURL(await response.blob())
  const anchor = document.createElement('a')
  anchor.href = url
  anchor.download = filename
  document.body.appendChild(anchor)
  anchor.click()
  anchor.remove()
  URL.revokeObjectURL(url)
}

export function setApiToken(value: string) {
  const clean = value.trim()
  if (clean) localStorage.setItem('recisdbApiToken', clean)
  else localStorage.removeItem('recisdbApiToken')
}

export function unwrapArray(value: unknown, keys: string[]): JsonRecord[] {
  if (Array.isArray(value)) return value as JsonRecord[]
  if (value && typeof value === 'object') {
    for (const key of keys) {
      const candidate = (value as JsonRecord)[key]
      if (Array.isArray(candidate)) return candidate as JsonRecord[]
    }
  }
  return []
}

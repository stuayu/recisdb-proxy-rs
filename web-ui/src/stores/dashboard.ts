import { defineStore } from 'pinia'
import { api, type JsonRecord, unwrapArray } from '../api'

export const useDashboardStore = defineStore('dashboard', {
  state: () => ({
    stats: {} as JsonRecord,
    clients: [] as JsonRecord[],
    loading: false,
    error: '',
    lastUpdated: 0,
    timer: 0 as number,
    eventAbort: null as AbortController | null,
  }),
  actions: {
    async refresh() {
      if (this.loading) return
      this.loading = true
      try {
        const [stats, clients] = await Promise.all([
          api<JsonRecord>('/stats'),
          api<unknown>('/clients'),
        ])
        this.stats = stats
        this.clients = unwrapArray(clients, ['clients', 'sessions', 'data'])
        this.error = ''
        this.lastUpdated = Date.now()
      } catch (error) {
        this.error = error instanceof Error ? error.message : String(error)
      } finally {
        this.loading = false
      }
    },
    start() {
      this.refresh()
      window.clearInterval(this.timer)
      // Slow fallback polling remains for proxies that buffer/disable SSE.
      this.timer = window.setInterval(() => this.refresh(), 30000)
      this.connectEvents()
    },
    async connectEvents() {
      this.eventAbort?.abort()
      const controller = new AbortController()
      this.eventAbort = controller
      const headers = new Headers({ Accept: 'text/event-stream' })
      const token = localStorage.getItem('recisdbApiToken')
      if (token) headers.set('Authorization', `Bearer ${token}`)
      try {
        const response = await fetch('/api/events', { headers, signal: controller.signal })
        if (!response.ok || !response.body) throw new Error(`SSE ${response.status}`)
        const reader = response.body.getReader()
        const decoder = new TextDecoder()
        let buffer = ''
        while (!controller.signal.aborted) {
          const result = await reader.read()
          if (result.done) break
          buffer += decoder.decode(result.value, { stream: true })
          const events = buffer.split('\n\n')
          buffer = events.pop() || ''
          if (events.some((event) => event.includes('event: refresh'))) await this.refresh()
        }
      } catch (error) {
        if (!controller.signal.aborted)
          console.warn('dashboard SSE unavailable; polling fallback remains active', error)
      }
    },
    stop() {
      window.clearInterval(this.timer)
      this.eventAbort?.abort()
      this.eventAbort = null
    },
  },
})

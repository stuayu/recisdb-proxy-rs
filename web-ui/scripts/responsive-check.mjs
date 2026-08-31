import { createServer } from 'node:http'
import { mkdir, readFile, stat } from 'node:fs/promises'
import { extname, join, normalize, resolve } from 'node:path'
import { chromium } from '@playwright/test'

const root = resolve(process.cwd(), '../recisdb-proxy/static/vue')
const output = resolve(process.cwd(), 'test-results/responsive')
let port = 0

const mockJson = {
  '/api/stats': { active_tuners: 2, active_sessions: 1, total_sessions: 12, total_channels: 3 },
  '/api/clients': { clients: [] },
  '/api/bondrivers': { bondrivers: [] },
  '/api/channels': {
    channels: [
      {
        id: 1,
        bon_driver_id: 1,
        channel_name: 'テスト総合',
        nid: 32736,
        sid: 101,
        tsid: 32736,
        priority: 10,
        is_enabled: true,
        bon_space: 0,
        bon_channel: 13,
      },
      {
        id: 2,
        bon_driver_id: 1,
        channel_name: 'テスト教育',
        nid: 32736,
        sid: 102,
        tsid: 32736,
        priority: 5,
        is_enabled: false,
        bon_space: 0,
        bon_channel: 13,
      },
    ],
  },
  '/api/client-view/targets': { targets: [] },
  '/api/scan-history': { history: [] },
  '/api/session-history': { history: [] },
  '/api/alerts': { alerts: [] },
  '/api/alert-rules': { rules: [] },
  '/api/scan-config': {},
  '/api/tuner-config': {},
  '/api/preview-config': {},
  '/api/tsreplace-config': {},
  '/api/nodes': {
    success: true,
    local: { node_id: 'home', display_name: '自宅' },
    nodes: [],
    route_groups: [],
    setup_status: [],
    topology: { local: { node_id: 'home', display_name: '自宅' }, nodes: [], paths: [] },
    pending_pairings: [],
  },
}

const contentTypes = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'application/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.woff2': 'font/woff2',
}

const server = createServer(async (request, response) => {
  const url = new URL(request.url || '/', `http://127.0.0.1:${port}`)
  if (url.pathname === '/api/events') {
    response.writeHead(204)
    response.end()
    return
  }
  const apiBody = mockJson[url.pathname]
  if (apiBody !== undefined) {
    response.writeHead(200, { 'Content-Type': 'application/json; charset=utf-8' })
    response.end(JSON.stringify(apiBody))
    return
  }
  if (url.pathname.startsWith('/api/')) {
    response.writeHead(200, { 'Content-Type': 'application/json; charset=utf-8' })
    response.end('{}')
    return
  }

  const requested = url.pathname === '/' ? 'index.html' : url.pathname.replace(/^\//, '')
  const normalized = normalize(requested)
  const file = join(root, normalized)
  if (!file.startsWith(root)) {
    response.writeHead(400)
    response.end('bad path')
    return
  }
  try {
    const info = await stat(file)
    const target = info.isDirectory() ? join(file, 'index.html') : file
    response.writeHead(200, {
      'Content-Type': contentTypes[extname(target)] || 'application/octet-stream',
    })
    response.end(await readFile(target))
  } catch {
    response.writeHead(404)
    response.end('not found')
  }
})

await mkdir(output, { recursive: true })
await new Promise((resolveReady, reject) => {
  server.once('error', reject)
  server.listen(0, '127.0.0.1', resolveReady)
})
const address = server.address()
if (!address || typeof address === 'string') throw new Error('responsive test server did not expose a TCP port')
port = address.port
const browser = await chromium.launch({ headless: true })
const tabs = [
  'overview',
  'bondrivers',
  'channels',
  'client-guide',
  'scan-history',
  'session-history',
  'alerts',
  'nodes',
  'settings',
]
const viewports = [
  { name: 'mobile', width: 390, height: 844 },
  { name: 'tablet', width: 768, height: 1024 },
  { name: 'desktop', width: 1280, height: 900 },
]
const failures = []

try {
  for (const viewport of viewports) {
    const page = await browser.newPage({ viewport })
    await page.goto(`http://127.0.0.1:${port}/`, { waitUntil: 'networkidle' })
    for (const tab of tabs) {
      await page.evaluate((id) => {
        location.hash = id
      }, tab)
      await page.waitForTimeout(100)
      const metrics = await page.evaluate(() => ({
        viewport: document.documentElement.clientWidth,
        body: document.body.scrollWidth,
        root: document.documentElement.scrollWidth,
      }))
      if (metrics.body > metrics.viewport + 1 || metrics.root > metrics.viewport + 1) {
        failures.push({ viewport: viewport.name, tab, ...metrics })
      }
    }
    await page.screenshot({ path: join(output, `${viewport.name}.png`), fullPage: true })
    await page.close()
  }
} finally {
  await browser.close()
  await new Promise((resolveClosed) => server.close(resolveClosed))
}

if (failures.length) {
  console.error(JSON.stringify(failures, null, 2))
  process.exit(1)
}
console.log('Responsive checks passed for 390px, 768px, and 1280px across all tabs.')

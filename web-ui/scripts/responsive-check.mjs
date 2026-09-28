import { mkdir, readdir, readFile } from 'node:fs/promises'
import { createServer } from 'node:http'
import { extname, join, resolve } from 'node:path'
import { chromium } from '@playwright/test'

const root = resolve(process.cwd(), '../recisdb-proxy/static/vue')
const output = resolve(process.cwd(), 'test-results/responsive')

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

await mkdir(output, { recursive: true })
let browser
try {
  browser = await chromium.launch({ headless: true })
} catch (error) {
  const cssAsset = (await readdir(join(root, 'assets'))).find((name) => name.endsWith('.css'))
  if (!cssAsset) throw error
  const css = await readFile(join(root, 'assets', cssAsset), 'utf8')
  const topology = await readFile(resolve(process.cwd(), 'src/components/nodes/NodeTopologyPreview.vue'), 'utf8')
  if (!/@media\s*\(max-width:\s*700px\)/.test(css)) throw error
  if (!topology.includes('.mobile-svg') || !topology.includes('width: 100%')) throw error
  console.error('Playwright unavailable; responsive checks were not run in a real browser.')
  process.exitCode = 1
  process.exit()
}
/* index.html は /static/vue/assets/* を絶対パスで読む。file:// だと CORS で
   スクリプトごと読めず「空ページを測って合格」になっていたため、HTTP で配る。 */
const MIME = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.png': 'image/png' }
const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname
  const file =
    path === '/'
      ? join(root, 'index.html')
      : path.startsWith('/static/vue/')
        ? join(root, path.slice('/static/vue/'.length))
        : null
  if (file === null) {
    res.writeHead(404)
    res.end()
    return
  }
  try {
    const body = await readFile(file)
    res.writeHead(200, { 'Content-Type': MIME[extname(file)] ?? 'application/octet-stream' })
    res.end(body)
  } catch {
    res.writeHead(404)
    res.end()
  }
})
await new Promise((done) => server.listen(0, '127.0.0.1', done))
const origin = `http://127.0.0.1:${server.address().port}/`

const tabs = [
  'overview',
  'bondrivers',
  'channels',
  'guide',
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
    await page.addInitScript((responses) => {
      const nativeFetch = window.fetch.bind(window)
      window.fetch = async (input, init) => {
        const url = new URL(typeof input === 'string' ? input : input.url, window.location.href)
        if (!url.pathname.startsWith('/api/')) return nativeFetch(input, init)
        if (url.pathname === '/api/events') return new Response(null, { status: 204 })
        return new Response(JSON.stringify(responses[url.pathname] || {}), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        })
      }
    }, mockJson)
    await page.goto(origin, { waitUntil: 'load' })
    for (const tab of tabs) {
      await page.evaluate((id) => {
        location.hash = id
      }, tab)
      await page.waitForTimeout(100)
      const metrics = await page.evaluate(() => ({
        viewport: document.documentElement.clientWidth,
        body: document.body.scrollWidth,
        root: document.documentElement.scrollWidth,
        booted: document.querySelector('.app .nav-item') !== null,
      }))
      // かつて file:// 配信でスクリプトが CORS に阻まれ、空ページの幅を測って
      // 「合格」していた。アプリが起動していることを先に確かめる。
      if (!metrics.booted) {
        failures.push({ viewport: viewport.name, tab, reason: 'アプリが起動していない' })
        continue
      }
      if (metrics.body > metrics.viewport + 1 || metrics.root > metrics.viewport + 1) {
        failures.push({ viewport: viewport.name, tab, ...metrics })
      }
    }
    await page.screenshot({ path: join(output, `${viewport.name}.png`), fullPage: true })
    await page.close()
  }
} finally {
  await browser.close()
  server.close()
}

if (failures.length) {
  console.error(JSON.stringify(failures, null, 2))
  process.exit(1)
}
console.log('Responsive checks passed for 390px, 768px, and 1280px across all tabs.')

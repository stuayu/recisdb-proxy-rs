import { mkdir, writeFile } from 'node:fs/promises'
import { basename, join, resolve } from 'node:path'
import { chromium } from '@playwright/test'

const baseUrl = process.env.BASE_URL || 'http://localhost:3000'
const base = new URL(baseUrl)
const defaultOut = join('test-results', 'live-audit', `${base.hostname}_${base.port || (base.protocol === 'https:' ? '443' : '80')}`)
const outDir = resolve(process.env.OUT_DIR || defaultOut)
const safeMethods = new Set(['GET', 'HEAD', 'OPTIONS'])
const streamPatterns = [
  /\/api\/stream\/service\/by-sid\//i,
  /\/api\/preview(?:\/|$)/i,
  /\/mirakurun\/api\/[^?]*\/stream/i,
  /(?:^|[/.])(?:m3u8|ts)(?:$|[?#])/i,
  /\/stream(?:\/|$)/i,
  /\/api\/(?:events|epg\/events)(?:$|[?#])/i,
]

const viewports = [
  { name: 'pc', width: 1440, height: 900, deviceScaleFactor: 1, hasTouch: false, isMobile: false },
  { name: 'tablet', width: 820, height: 1180, deviceScaleFactor: 2, hasTouch: true, isMobile: false },
  { name: 'phone', width: 390, height: 844, deviceScaleFactor: 3, hasTouch: true, isMobile: true },
]

const tabs = [
  ['overview', '概要'], ['bondrivers', 'BonDriver'], ['channels', 'チャンネル'],
  ['nodes', '分散ノード'], ['guide', '番組表'], ['client-guide', 'クライアント設定'],
  ['scan-history', 'スキャン履歴'], ['session-history', 'セッション履歴'],
  ['alerts', 'アラート'], ['settings', '設定'], ['encode-profiles', 'エンコード'], ['logs', 'ログ'],
]
const settings = [
  ['EPG自動取得', '/settings/epg'], ['スキャンスケジューラー', '/scan-config'],
  ['チューナー最適化', '/tuner-config'], ['BNDP外部エンコード（tsreplace）', '/tsreplace-config'],
  ['ブラウザプレビュー', '/preview-config'], ['番組表', '/guide-config'], ['ログ出力', '/log-config'],
]

const report = { base_url: base.href, generated_at: new Date().toISOString(), sources: {
  tabs: 'web-ui/src/App.vue:17-29', settings: 'web-ui/src/components/SettingsView.vue:5-12',
  preview_stream: 'web-ui/src/components/PreviewPlayer.vue:70-73',
  epg_sse: 'web-ui/src/composables/useEpgEvents.ts:185-191', dashboard_sse: 'web-ui/src/stores/dashboard.ts:46-50',
}, screens: [], aborted_writes: [], issues: [] }
let currentScreen = 'startup'
let currentAction = '初期化'
let screenshotIndex = 0

const clean = (value, max = 90) => String(value ?? '').replace(/\s+/g, ' ').trim().slice(0, max)
const slug = (value) => String(value).replace(/[^\p{Letter}\p{Number}_-]+/gu, '-').replace(/^-+|-+$/g, '').slice(0, 80) || 'screen'
const isStream = (url, resourceType = '') => resourceType === 'websocket' || streamPatterns.some((pattern) => pattern.test(url))
const issue = (screen, severity, kind, detail, screenshot = null) => {
  const item = { severity, kind, detail }
  if (screenshot) item.screenshot = basename(screenshot)
  report.issues.push({ screen, ...item })
  return item
}

function ensureRecord(viewport, screen, label = screen) {
  let record = report.screens.find((entry) => entry.viewport === viewport && entry.screen === screen)
  if (!record) {
    record = { viewport, screen, label, screenshots: [], issues: [], aborted_writes: [], metrics: {} }
    report.screens.push(record)
  }
  return record
}

async function screenshot(page, screen, label) {
  const file = join(outDir, currentViewport, `${slug(screen)}-${String(screenshotIndex++).padStart(2, '0')}-${slug(label)}.png`)
  await page.screenshot({ path: file, fullPage: true })
  ensureRecord(currentViewport, screen).screenshots.push(file)
  return file
}

async function inspect(page, screen, shot, viewportName) {
  const result = await page.evaluate(() => {
    const text = (node) => (node?.textContent || '').replace(/\s+/g, ' ').trim()
    const visible = (node) => {
      const r = node.getBoundingClientRect(); const s = getComputedStyle(node)
      return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none'
    }
    const root = document.documentElement
    const overflow = [...document.querySelectorAll('*')].filter((node) => {
      const r = node.getBoundingClientRect(); return visible(node) && r.right > root.clientWidth + 1
    }).sort((a, b) => b.getBoundingClientRect().right - a.getBoundingClientRect().right).slice(0, 5)
      .map((node) => ({ tag: node.tagName, class: node.className || '', text: text(node).slice(0, 40), right: Math.round(node.getBoundingClientRect().right) }))
    const tapTargets = [...document.querySelectorAll('button, a, input, select, [role="button"], [role="tab"]')]
      .filter(visible).map((node) => ({ node, r: node.getBoundingClientRect() }))
      .filter(({ r }) => r.width < 32 || r.height < 32).slice(0, 10)
      .map(({ node, r }) => ({ tag: node.tagName, class: node.className || '', text: text(node).slice(0, 40), width: Math.round(r.width), height: Math.round(r.height) }))
    const clipped = [...document.querySelectorAll('*')].filter((node) => {
      const s = getComputedStyle(node); return visible(node) && node.scrollWidth > node.clientWidth + 1 && s.overflow === 'hidden' && s.textOverflow !== 'ellipsis'
    }).slice(0, 10).map((node) => ({ tag: node.tagName, class: node.className || '', text: text(node).slice(0, 40) }))
    const main = document.querySelector('main')
    const mainText = text(main)
    const loading = /読み込み中|取得中|更新中|準備中|テスト中/.test(mainText)
    const badText = [...new Set((mainText.match(/undefined|NaN|\[object Object\]|Invalid Date|(?:^|\s)null(?:\s|$)/g) || []).map((x) => x.trim()))]
    return {
      document_width: root.clientWidth, scroll_width: root.scrollWidth,
      overflow, tapTargets, clipped, main_text_length: mainText.length, loading,
      badText, epoch1970: /1970/.test(mainText),
      guideCellCount: document.querySelectorAll('.guide-cell').length,
      guideColumnCount: document.querySelectorAll('.guide-col').length,
      guideEmptyColumns: [...document.querySelectorAll('.guide-col')].filter((col) => !col.querySelector('.guide-cell')).length,
      epgText: [...document.querySelectorAll('.epg-status, .guide-epg-status-notice, .guide-epg-status')].filter(visible).map(text).filter(Boolean),
    }
  })
  const record = ensureRecord(viewportName, screen)
  record.metrics = { ...(record.metrics || {}), ...result }
  if (result.scroll_width > result.document_width + 1) issue(screen, 'medium', '横スクロール', JSON.stringify(result.overflow), shot)
  if (viewportName !== 'pc' && result.tapTargets.length) issue(screen, 'medium', '小さいタップ対象', JSON.stringify(result.tapTargets), shot)
  if (result.clipped.length) issue(screen, 'low', '文字切れの可能性', JSON.stringify(result.clipped), shot)
  if (result.main_text_length < 20) issue(screen, 'high', '空表示', `main text length=${result.main_text_length}`, shot)
  if (result.loading) issue(screen, 'high', '読み込み表示が残存', '最大待機後も読み込み相当の表示が残った', shot)
  if (result.badText.length) issue(screen, 'high', '未解決プレースホルダ', result.badText.join(', '), shot)
  if (result.epoch1970) issue(screen, 'low', '1970年の日付', '画面テキストに1970がある', shot)
  if (result.epgText.length) {
    record.metrics.epg_text = result.epgText
    const epgFile = join(outDir, currentViewport, `${slug(screen)}-epg-status.png`)
    const epg = page.locator('.epg-status, .guide-epg-status-notice, .guide-epg-status').first()
    if (await epg.count()) { await epg.screenshot({ path: epgFile }); record.screenshots.push(epgFile) }
  }
  return result
}

async function waitForScreen(page) {
  await page.waitForTimeout(500)
  const deadline = Date.now() + 15000
  while (Date.now() < deadline) {
    const loading = await page.locator('main').textContent().then((text) => /読み込み中|取得中|更新中/.test(text || '')).catch(() => false)
    if (!loading) return
    await page.waitForTimeout(400)
  }
}

async function doAction(page, screen, label, callback) {
  if (currentActionCount >= 40) return false
  currentActionCount++
  currentAction = label
  try { await Promise.race([callback(), new Promise((_, reject) => setTimeout(() => reject(new Error('操作タイムアウト')), 5000))]) }
  catch (error) { issue(screen, 'high', '操作例外', `${label}: ${clean(error?.message || error)}`) }
  await page.waitForTimeout(150)
  const shot = await screenshot(page, screen, `${String(currentActionCount).padStart(2, '0')}-${label}`)
  await inspect(page, screen, shot, currentViewport)
  return true
}

async function commonInteractions(page, screen) {
  const details = page.locator('main details')
  for (let i = 0; i < Math.min(await details.count(), 8); i++) {
    await doAction(page, screen, `詳細-${i + 1}`, async () => { await details.nth(i).locator('summary').click(); if (await details.nth(i).getAttribute('open')) await details.nth(i).locator('summary').click() })
  }
  const candidates = page.locator('main button, main [role="button"]')
  const count = Math.min(await candidates.count(), 40 - currentActionCount)
  for (let i = 0; i < count; i++) {
    const button = candidates.nth(i)
    if (!await button.isVisible().catch(() => false)) continue
    const label = clean(await button.innerText().catch(() => button.getAttribute('aria-label')), 30)
    if (!label || /CSV出力|CSV読込|ダウンロード|ログファイル/.test(label) || /視聴|プレビュー/.test(label)) continue
    if (!/更新|再読み込み|再取得|検索|編集|詳細|設定|構成図|経路|確認|保存|適用|切断|削除|追加|新規|スキャン|有効化|停止|開く|次|前|ソート|現在時刻|今日|地上|BS|CS|4K|局|時間|サブCH|表示設定/.test(label)) continue
    await doAction(page, screen, label, async () => { await button.click() })
  }
}

async function tableInteractions(page, screen) {
  const search = page.locator('main input[type="search"], main input[placeholder*="検索"], main input[placeholder*="名称"]')
  if (await search.count()) await doAction(page, screen, '検索入力', async () => { await search.first().fill('監査'); await search.first().press('End') })
  const sort = page.locator('main th button, main .sort-button')
  if (await sort.count()) await doAction(page, screen, 'ヘッダーソート', async () => { await sort.first().click() })
  const picker = page.locator('main details.column-picker')
  if (await picker.count()) await doAction(page, screen, '列表示切替', async () => { await picker.first().locator('summary').click(); const input = picker.first().locator('input').first(); if (await input.count()) await input.click() })
  const pager = page.locator('main button').filter({ hasText: /次へ|前へ|ページ/ })
  for (let i = 0; i < Math.min(2, await pager.count()); i++) await doAction(page, screen, `ページング-${i + 1}`, async () => { await pager.nth(i).click() })
}

async function guideInteractions(page, screen) {
  for (const selector of ['.guide-band-tab', '.guide-density-button', '[aria-label="現在時刻へ"]', '.guide-filters-toggle']) {
    const loc = page.locator(selector)
    for (let i = 0; i < Math.min(await loc.count(), 4); i++) await doAction(page, screen, `${selector.replace(/[^a-z]+/gi, '-')}-${i + 1}`, async () => { await loc.nth(i).click() })
  }
  const region = page.locator('.guide-region-filter select')
  if (await region.count() && await region.first().isEnabled().catch(() => false)) await doAction(page, screen, '地域切替', async () => { const options = await region.first().locator('option').allTextContents(); if (options.length > 1) await region.first().selectOption({ index: 1 }) })
  const query = page.locator('.guide-service-search input')
  if (await query.count()) await doAction(page, screen, '局検索', async () => { await query.fill('NHK') })
  const scroll = page.locator('.guide-scroll')
  if (await scroll.count()) {
    await doAction(page, screen, '番組表縦横スクロール', async () => { await scroll.evaluate((node) => { node.scrollTop = Math.min(node.scrollHeight, 900); node.scrollLeft = Math.min(node.scrollWidth, 700) }) })
    const cell = page.locator('.guide-cell').first()
    if (await cell.count()) await doAction(page, screen, '番組セル詳細', async () => { await cell.click() })
    const dialog = page.locator('[role="dialog"]:visible')
    if (await dialog.count()) await doAction(page, screen, '番組詳細を閉じる', async () => { await page.keyboard.press('Escape'); const close = dialog.locator('button').filter({ hasText: '閉じる' }); if (await close.count()) await close.first().click() })
  }
}

async function settingsInteractions(page, screen) {
  const select = page.locator('main select').first()
  if (!await select.count()) return
  for (const [label, value] of settings) {
    currentScreen = `${screen}-${label}`
    await doAction(page, `${screen}-${label}`, `設定カテゴリ-${label}`, async () => { await select.selectOption(value); await waitForScreen(page) })
    const sub = `${screen}-${label}`
    await tableInteractions(page, sub)
    if (value === '/settings/epg') {
      const input = page.locator('main input[type="number"]').first()
      if (await input.count()) {
        await doAction(page, sub, 'EPG値変更保存', async () => { const old = await input.inputValue(); await input.fill(old || '90'); await page.getByRole('button', { name: '保存', exact: true }).last().click() })
        const error = page.locator('main [role="alert"], main .notice.error')
        if (!await error.count()) issue(sub, 'medium', 'abort後のエラー表示なし', 'EPG設定保存のPUTがabortされた後、利用者向けエラー表示を検出できない')
      }
    }
    await commonInteractions(page, sub)
  }
}

let currentViewport = 'unknown'
let currentActionCount = 0

async function main() {
  await mkdir(outDir, { recursive: true })
  const browser = await chromium.launch({ headless: true })
  try {
    for (const viewport of viewports) {
      currentViewport = viewport.name
      const page = await browser.newPage({ viewport, deviceScaleFactor: viewport.deviceScaleFactor, hasTouch: viewport.hasTouch, isMobile: viewport.isMobile })
      const aborted = []
      const consoleErrors = []
      page.on('dialog', (dialog) => void dialog.dismiss())
      page.on('console', (message) => { if (message.type() === 'error') consoleErrors.push(message.text()) })
      page.on('pageerror', (error) => consoleErrors.push(`pageerror: ${error.message}`))
      page.on('response', (response) => { if (response.request().method() === 'GET' && response.status() >= 400) issue(currentScreen, 'high', '失敗したGET', `${response.status()} ${response.url()}`) })
      await page.route('**/*', async (route) => {
        const request = route.request(); const url = request.url(); const method = request.method()
        if (isStream(url, request.resourceType()) || !safeMethods.has(method)) {
          const entry = { method, url, screen: currentScreen, action: currentAction }
          aborted.push(entry)
          report.aborted_writes.push({ viewport: currentViewport, ...entry })
          ensureRecord(currentViewport, currentScreen).aborted_writes.push({ viewport: currentViewport, ...entry })
          await route.abort(); return
        }
        await route.continue()
      })
      await page.goto(base.href, { waitUntil: 'domcontentloaded', timeout: 15000 }).catch((error) => issue('startup', 'high', '初期表示失敗', clean(error.message)))
      for (const [id, label] of tabs) {
        currentScreen = id; currentActionCount = 0; screenshotIndex = 0
        await page.evaluate((tab) => { location.hash = tab }, id)
        await waitForScreen(page)
        const screen = ensureRecord(currentViewport, id, label)
        const initial = await screenshot(page, id, 'initial')
        const metrics = await inspect(page, id, initial, currentViewport)
        screen.metrics = { ...screen.metrics, ...metrics }
        if (id === 'guide') await guideInteractions(page, id)
        else if (id === 'settings') await settingsInteractions(page, id)
        else { await tableInteractions(page, id); await commonInteractions(page, id) }
        aborted.splice(0)
        if (consoleErrors.length) { screen.metrics.console_errors = [...consoleErrors]; issue(id, 'high', 'コンソールエラー', consoleErrors.join('\n'), initial); consoleErrors.length = 0 }
      }
      await page.close()
    }
  } finally { await browser.close() }
  for (const entry of report.screens) entry.issues = report.issues.filter((item) => item.screen === entry.screen || item.screen.startsWith(`${entry.screen}-`))
  const issueLines = report.issues.length ? report.issues.map((item) => `- [${item.severity}] ${item.screen}: ${item.kind} — ${item.detail}${item.screenshot ? ` (${item.screenshot})` : ''}`).join('\n') : '- 問題なし'
  const md = `# Live UI audit\n\n- 対象: ${report.base_url}\n- 実行時刻: ${report.generated_at}\n- 画面数: ${report.screens.length}\n- 中断リクエスト: ${report.aborted_writes.length}\n\n## 問題\n\n${issueLines}\n\n## 中断した書き込み・ストリーム\n\n${report.aborted_writes.length ? report.aborted_writes.map((item) => `- [${item.viewport}] ${item.method} ${item.url} — ${item.screen} / ${item.action}`).join('\n') : '- なし'}\n\n## 根拠\n\n- タブ: ${report.sources.tabs}\n- 設定カテゴリ: ${report.sources.settings}\n- プレビュー: ${report.sources.preview_stream}\n- EPG SSE: ${report.sources.epg_sse}\n- Dashboard SSE: ${report.sources.dashboard_sse}\n\n## 既知の限界\n\n- 実サーバーのデータ量・認証・権限・端末固有描画は実行時環境に依存。\n- 書き込みはリクエスト送信前にabortするため、サーバー側の実処理結果は検証しない。\n- ダウンロード・ファイル選択・プレビュー再生は実行しない。\n- 操作は各画面40件、各操作5秒、初期表示15秒で打ち切る。\n`
  await writeFile(join(outDir, 'report.json'), `${JSON.stringify(report, null, 2)}\n`)
  await writeFile(join(outDir, 'report.md'), md)
}

try { await main() } catch (error) {
  report.issues.push({ screen: currentScreen, severity: 'high', kind: '監査スクリプト例外', detail: clean(error?.stack || error) })
  await mkdir(outDir, { recursive: true })
  await writeFile(join(outDir, 'report.json'), `${JSON.stringify(report, null, 2)}\n`)
  await writeFile(join(outDir, 'report.md'), `# Live UI audit\n\n監査スクリプト例外: ${clean(error?.message || error)}\n`)
} finally { process.exitCode = 0 }

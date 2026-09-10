// 番組表 (GuideView) の実ブラウザ検証。本番規模のモックEPGを食わせて、
// 仮想化・sticky・状態表現・キーボード操作・レスポンシブを確認する。
import { mkdir, readFile } from 'node:fs/promises'
import { createServer } from 'node:http'
import { extname, join, resolve } from 'node:path'
import { chromium } from '@playwright/test'

const root = resolve(process.cwd(), '../recisdb-proxy/static/vue')
const output = resolve(process.cwd(), 'test-results/guide')

// GuideView.vue の GRID_START_HOUR=6 と useGuideDate.broadcastDateInput に合わせる。
const GUIDE_GRID_START_HOUR = 6
const FIXED_NOW = new Date('2026-01-15T12:00:00')
function broadcastDateStart(timestamp) {
  const date = new Date(timestamp)
  if (date.getHours() < GUIDE_GRID_START_HOUR) date.setDate(date.getDate() - 1)
  date.setHours(GUIDE_GRID_START_HOUR, 0, 0, 0)
  return date
}

// 本番相当: 地上30 + BS20 + CS20 = 70サービス、各24時間ぶんの番組 (約 70*40 = 2800件)
const CH_NAMES = ['NHK総合', 'NHK Eテレ', '日本テレビ', 'テレビ朝日', 'TBS', 'テレビ東京', 'フジテレビ', 'TOKYO MX', 'tvk', 'チバテレ']
const channels = []
const programs = []
let pid = 1
const dayStart = broadcastDateStart(FIXED_NOW)
const base = Math.floor(dayStart.getTime() / 1000)

function pushService(nid, sid, tsid, name, band, rck, region) {
  channels.push({
    id: channels.length + 1, bon_driver_id: 1, channel_name: name,
    nid, sid, tsid, band_type: band, service_type: 1,
    remote_control_key: rck, terrestrial_region: region,
    priority: 10, is_enabled: true, bon_space: 0, bon_channel: 13,
  })
  // 30分〜2時間のランダムでない決定的な長さで24時間を埋める
  let t = base
  let k = 0
  while (t < base + 24 * 3600) {
    const dur = [1800, 3600, 5400, 900, 2700][(sid + k) % 5]
    programs.push({
      id: pid++, nid, sid, event_id: 1000 + k, start_at: t, duration_secs: dur,
      name: `${name} 番組${k + 1}`,
      description: `これは ${name} の番組${k + 1} の説明文です。ジャンル別の色分け確認用。`,
      extended: '', genre: ((sid + k) % 12) << 4,
    })
    t += dur; k++
  }
}
for (let i = 0; i < 30; i++) {
  pushService(0x7880 + i, 1024 + i * 8, 0x7880 + i, `${CH_NAMES[i % 10]}${i < 10 ? '' : i}`, 0, (i % 12) + 1, i < 15 ? '関東' : '東北')
}
for (let i = 0; i < 20; i++) pushService(4, 101 + i, 0x4000 + i, `BS局${i + 1}`, 1, null, null)
for (let i = 0; i < 20; i++) pushService(6, 201 + i, 0x6000 + i, `CS局${i + 1}`, 2, null, null)

const mock = {
  '/api/stats': { active_tuners: 2, active_sessions: 1, total_sessions: 12, total_channels: channels.length },
  '/api/clients': { clients: [] },
  '/api/bondrivers': { bondrivers: [] },
  '/api/alerts': { alerts: [] },
  '/api/nodes': { success: true, local: {}, nodes: [], route_groups: [], setup_status: [], topology: { local: {}, nodes: [], paths: [] }, pending_pairings: [] },
}

await mkdir(output, { recursive: true })

/* index.html は /static/vue/assets/* を絶対パスで読む。file:// だと CORS で
   スクリプトごと落ちて「空ページを測って合格」になるため、HTTP で配る。 */
const MIME = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.png': 'image/png' }
const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname
  const file = path === '/' ? join(root, 'index.html')
    : path.startsWith('/static/vue/') ? join(root, path.slice('/static/vue/'.length))
    : null
  if (file === null) { res.writeHead(404); res.end(); return }
  try {
    const body = await readFile(file)
    res.writeHead(200, { 'Content-Type': MIME[extname(file)] ?? 'application/octet-stream' })
    res.end(body)
  } catch { res.writeHead(404); res.end() }
})
await new Promise((done) => server.listen(0, '127.0.0.1', done))
const origin = `http://127.0.0.1:${server.address().port}/`

const browser = await chromium.launch({ headless: true })
const failures = []
const notes = []

async function guideDiagnostics(page) {
  return page.evaluate(() => ({
    emptyState: [...document.querySelectorAll('.empty-state')].map((node) => node.textContent?.trim()).filter(Boolean),
    selectedDate: document.querySelector('.guide-date-picker input')?.value ?? null,
    guideScroll: !!document.querySelector('.guide-scroll'),
    guideCellCount: document.querySelectorAll('.guide-cell').length,
    programFetches: globalThis.__guideProgramFetches ?? [],
  }))
}

for (const vp of [
  { name: '1920x1080', width: 1920, height: 1080 },
  { name: '1280x720', width: 1280, height: 720 },
  { name: '768', width: 768, height: 1024 },
  { name: '390', width: 390, height: 844 },
]) {
  for (const theme of vp.name === '1280x720' ? ['light', 'dark'] : ['light']) {
    const page = await browser.newPage({ viewport: { width: vp.width, height: vp.height } })
    await page.clock.install({ time: FIXED_NOW })
    const consoleErrors = []
    // ロゴ画像は本番にしか無い。404 は onLogoError が握るので雑音として除き、
    // それ以外の 404 とスクリプト例外だけを失敗として拾う。
    page.on('console', (m) => {
      if (m.type() !== 'error') return
      if (m.text().startsWith('Failed to load resource')) return
      consoleErrors.push(m.text())
    })
    page.on('response', (r) => {
      if (r.status() >= 400 && !r.url().includes('/logos/')) {
        consoleErrors.push(`${r.status()} ${r.url()}`)
      }
    })
    page.on('pageerror', (e) => consoleErrors.push(`pageerror: ${e.message}`))
    await page.addInitScript(([responses, chs, progs, wantDark]) => {
      if (wantDark) localStorage.setItem('dashboardTheme', 'dark')
      globalThis.__guideProgramFetches = []
      const nativeFetch = window.fetch.bind(window)
      window.fetch = async (input, init) => {
        const url = new URL(typeof input === 'string' ? input : input.url, window.location.href)
        if (!url.pathname.startsWith('/api/')) return nativeFetch(input, init)
        if (url.pathname === '/api/events') return new Response(null, { status: 204 })
        let body = responses[url.pathname] || {}
        if (url.pathname === '/api/channels') {
          body = { channels: chs, total: chs.length, count: chs.length }
        }
        if (url.pathname === '/api/programs') {
          const since = Number(url.searchParams.get('since') || 0)
          const until = Number(url.searchParams.get('until') || 0)
          const svc = url.searchParams.get('services')
          const set = svc ? new Set(svc.split(',')) : null
          const matched = progs.filter((p) =>
            p.start_at + p.duration_secs > since && p.start_at < until &&
            (set === null || set.has(`${p.nid}:${p.sid}`)))
          globalThis.__guideProgramFetches.push({ since, until, services: svc, count: matched.length })
          body = { programs: matched }
        }
        return new Response(JSON.stringify(body), { status: 200, headers: { 'Content-Type': 'application/json' } })
      }
    }, [mock, channels, programs, theme === 'dark'])
    await page.goto(origin, { waitUntil: 'load' })
    await page.evaluate(() => { location.hash = 'guide' })
    try {
      await page.waitForSelector('.guide-scroll', { timeout: 10000 })
    } catch (cause) {
      const diagnostics = await guideDiagnostics(page)
      console.error(`GuideView の描画待機に失敗: ${JSON.stringify(diagnostics, null, 2)}`)
      throw cause
    }
    await page.waitForTimeout(1200)

    const initialDiagnostics = await guideDiagnostics(page)
    if (initialDiagnostics.guideCellCount === 0) {
      console.error(`GuideView に番組セルがない: ${JSON.stringify(initialDiagnostics, null, 2)}`)
      throw new Error('GuideView rendered zero program cells')
    }

    const tag = `${vp.name}-${theme}`
    const m = await page.evaluate(() => {
      const scroll = document.querySelector('.guide-scroll')
      const cs = scroll ? getComputedStyle(scroll) : null
      const cells = document.querySelectorAll('.guide-cell')
      const cols = document.querySelectorAll('.guide-col')
      const header = document.querySelector('.guide-header-row')
      const axis = document.querySelector('.guide-timeaxis')
      const corner = document.querySelector('.guide-corner')
      const grid = document.querySelector('.guide-grid')
      const rect = scroll?.getBoundingClientRect()
      return {
        pageOverflowX: document.documentElement.scrollWidth - document.documentElement.clientWidth,
        scrollOverflow: cs?.overflow, scrollHeight: rect?.height,
        guideShareOfViewport: rect ? Math.round((rect.height / window.innerHeight) * 100) : 0,
        cellCount: cells.length, colCount: cols.length,
        gridWidth: grid ? parseFloat(getComputedStyle(grid).width) : 0,
        headerSticky: header ? getComputedStyle(header).position : null,
        axisSticky: axis ? getComputedStyle(axis).position : null,
        cornerSticky: corner ? getComputedStyle(corner).position : null,
        cellAbsolute: cells[0] ? getComputedStyle(cells[0]).position : null,
        hourAbsolute: document.querySelector('.guide-hour-label')
          ? getComputedStyle(document.querySelector('.guide-hour-label')).position : null,
        nowLine: !!document.querySelector('.guide-now-line'),
        nowBadge: document.querySelector('.guide-now-badge')?.textContent ?? null,
        onAir: document.querySelectorAll('.guide-cell-onair').length,
        chNum: document.querySelector('.guide-ch-num')?.textContent ?? null,
        bandTabs: document.querySelectorAll('.guide-band-tab').length,
        densityButtons: document.querySelectorAll('.guide-density-button').length,
        actionbar: document.querySelectorAll('.guide-actionbar .guide-chip-button').length,
        isDark: document.querySelector('.app')?.classList.contains('dark') ?? false,
        cellBg: (() => {
          const c = document.querySelector('.guide-cell')
          return c ? getComputedStyle(c).backgroundColor : null
        })(),
        visibleCols: (() => {
          if (!scroll) return 0
          const r = scroll.getBoundingClientRect()
          let n = 0
          for (const h of document.querySelectorAll('.guide-header-cell')) {
            const b = h.getBoundingClientRect()
            if (b.left >= r.left - 1 && b.right <= r.right + 1) n++
          }
          return n
        })(),
      }
    })
    notes.push({ tag, ...m })
    if (m.pageOverflowX > 1) failures.push(`${tag}: ページに横スクロール ${m.pageOverflowX}px`)
    if (m.scrollOverflow !== 'auto') failures.push(`${tag}: .guide-scroll overflow=${m.scrollOverflow}`)
    if (m.cellAbsolute !== 'absolute') failures.push(`${tag}: .guide-cell position=${m.cellAbsolute}`)
    if (m.hourAbsolute !== 'absolute') failures.push(`${tag}: .guide-hour-label position=${m.hourAbsolute}`)
    if (m.headerSticky !== 'sticky') failures.push(`${tag}: header not sticky`)
    if (m.axisSticky !== 'sticky') failures.push(`${tag}: axis not sticky`)
    if (m.cornerSticky !== 'sticky') failures.push(`${tag}: corner not sticky`)
    if (m.colCount >= 60) failures.push(`${tag}: 列が仮想化されていない (${m.colCount}/70)`)
    if (m.cellCount > 1200) failures.push(`${tag}: セル数が多すぎる ${m.cellCount}`)
    if (!m.nowLine) failures.push(`${tag}: 現在時刻ラインなし`)
    if (m.onAir === 0) failures.push(`${tag}: 放送中セルなし`)
    if (m.bandTabs !== 4) failures.push(`${tag}: バンドタブ ${m.bandTabs}`)
    // 局数 (狭幅は 9 局を出さない) + 時間幅 + サブCH の切替ボタン
    const wantDensity = (vp.width <= 700 ? 5 : 6) + 1
    if (m.densityButtons !== wantDensity) {
      failures.push(`${tag}: 密度ボタン ${m.densityButtons} (期待 ${wantDensity})`)
    }
    if ((theme === 'dark') !== m.isDark) failures.push(`${tag}: テーマが当たっていない (isDark=${m.isDark})`)

    await page.screenshot({ path: join(output, `guide-${tag}.png`) })

    if (vp.name === '1280x720' && theme === 'light') {
      // --- 対話確認 ---
      // クリック → 選択 + 詳細ダイアログ
      // クリックは「選択」だけ。詳細は常設ペインに出る (REGZA の番組表と同じ)。
      await page.locator('.guide-cell').nth(3).click()
      await page.waitForTimeout(600)
      const afterClick = await page.evaluate(() => ({
        dialog: !!document.querySelector('.guide-detail-dialog'),
        selected: document.querySelectorAll('.guide-cell-selected').length,
        actionLabel: document.querySelector('.guide-actionbar-selected')?.textContent?.trim() ?? '',
        paneTitle: document.querySelector('.guide-detail-pane-heading h2')?.textContent?.trim() ?? '',
        paneCopy: document.querySelector('.guide-detail-pane-copy')?.textContent?.trim() ?? '',
      }))
      if (afterClick.dialog) failures.push('クリックだけで詳細ダイアログが開いている')
      if (afterClick.selected !== 1) failures.push(`クリック後の選択セル数 ${afterClick.selected}`)
      if (!afterClick.paneTitle) failures.push('詳細ペインに番組名が出ていない')
      if (!afterClick.paneCopy) failures.push('詳細ペインに説明文が出ていない (遅延取得が効いていない)')
      if (!afterClick.actionLabel || afterClick.actionLabel.includes('番組を選ぶと')) {
        failures.push('操作バーに選択番組名が出ていない')
      }
      // Enter → ダイアログ、Escape → 閉じる
      await page.locator('.guide-scroll').focus()
      await page.keyboard.press('Enter'); await page.waitForTimeout(300)
      if (!(await page.locator('.guide-detail-dialog').count())) failures.push('Enterで詳細ダイアログが開かない')
      await page.screenshot({ path: join(output, 'guide-detail.png') })
      await page.keyboard.press('Escape'); await page.waitForTimeout(250)
      if (await page.locator('.guide-detail-dialog').count()) failures.push('Escapeで詳細が閉じない')
      // 矢印キー → 選択移動
      const before = await page.evaluate(() =>
        document.querySelector('.guide-cell-selected')?.getAttribute('aria-label') ?? null)
      await page.locator('.guide-cell-selected').focus()
      await page.keyboard.press('ArrowDown'); await page.waitForTimeout(250)
      const afterDown = await page.evaluate(() =>
        document.querySelector('.guide-cell-selected')?.getAttribute('aria-label') ?? null)
      if (before === afterDown || afterDown === null) failures.push(`ArrowDownで選択が動かない (${before} → ${afterDown})`)
      await page.keyboard.press('ArrowRight'); await page.waitForTimeout(300)
      const afterRight = await page.evaluate(() =>
        document.querySelector('.guide-cell-selected')?.getAttribute('aria-label') ?? null)
      if (afterRight === afterDown || afterRight === null) failures.push(`ArrowRightで選択が動かない (${afterDown} → ${afterRight})`)
      // 密度切替: 局数を変えると 1 画面に収まる列数が変わる
      const colsBefore = await page.evaluate(() => document.querySelectorAll('.guide-header-cell').length)
      await page.locator('.guide-density-button', { hasText: '9局' }).click()
      await page.waitForTimeout(500)
      const after9 = await page.evaluate(() => ({
        width: parseFloat(getComputedStyle(document.querySelector('.guide-header-cell')).width),
        cols: document.querySelectorAll('.guide-header-cell').length,
        stored: localStorage.getItem('guide:density'),
      }))
      if (!after9.stored || !after9.stored.includes('9')) failures.push(`局数が localStorage に残らない (${after9.stored})`)
      if (!(after9.cols >= colsBefore)) failures.push(`9局にしても列が増えない (${colsBefore} → ${after9.cols})`)
      const hourBefore = await page.evaluate(() =>
        parseFloat(getComputedStyle(document.querySelector('.guide-grid')).getPropertyValue('--guide-hour-h')))
      await page.locator('.guide-density-button', { hasText: '3時間' }).click()
      await page.waitForTimeout(500)
      const hourAfter = await page.evaluate(() =>
        parseFloat(getComputedStyle(document.querySelector('.guide-grid')).getPropertyValue('--guide-hour-h')))
      if (!(hourAfter > hourBefore)) failures.push(`3時間にしても 1 時間の高さが伸びない (${hourBefore} → ${hourAfter})`)
      await page.locator('.guide-density-button', { hasText: '7局' }).click()
      await page.locator('.guide-density-button', { hasText: '4時間' }).click()
      await page.waitForTimeout(400)
      // 密度ボタンにフォーカスが残ったままだと、以降の Enter がボタンの再押下になる。
      await page.locator('.guide-cell-selected').first().focus()

      const states = await page.evaluate(() => {
        const pick = (el) => {
          if (!el) return null
          const c = getComputedStyle(el)
          return { outline: c.outlineWidth + ' ' + c.outlineStyle + ' ' + c.outlineColor, boxShadow: c.boxShadow, zIndex: c.zIndex }
        }
        const sel = document.querySelector('.guide-cell-selected')
        const onair = document.querySelector('.guide-cell-onair:not(.guide-cell-selected)')
        const plain = [...document.querySelectorAll('.guide-cell')].find(
          (c) => !c.classList.contains('guide-cell-selected') && !c.classList.contains('guide-cell-onair'))
        return { selected: pick(sel), onair: pick(onair), plain: pick(plain) }
      })
      notes.push({ tag: 'states', ...states })
      if (!states.selected || states.selected.outline === states.plain?.outline) {
        failures.push(`選択セルの outline が通常セルと同じ (${JSON.stringify(states.selected)})`)
      }
      if (states.selected && !/^3px solid/.test(states.selected.outline)) {
        failures.push(`選択セルの outline が 3px でない: ${states.selected.outline}`)
      }
      if (!states.onair || states.onair.boxShadow === states.plain?.boxShadow) {
        failures.push(`放送中セルの box-shadow が通常セルと同じ (${JSON.stringify(states.onair)})`)
      }
      await page.screenshot({ path: join(output, 'guide-selected.png') })
      // Enter → 詳細
      await page.keyboard.press('Enter'); await page.waitForTimeout(300)
      if (!(await page.locator('.guide-detail-dialog').count())) failures.push('Enterで詳細が開かない')
      await page.keyboard.press('Escape'); await page.waitForTimeout(200)
      // 「視聴」→ PreviewPlayer が載ることを確認 (実ストリームは流れないので mount まで)
      await page.locator('.guide-actionbar .guide-chip-button', { hasText: '視聴' }).click()
      await page.waitForTimeout(600)
      if (!(await page.locator('.preview-dialog').count())) failures.push('「視聴」でプレビューが開かない')
      await page.screenshot({ path: join(output, 'guide-preview.png') })
      await page.keyboard.press('Escape')
      await page.waitForTimeout(300)
      if (await page.locator('.preview-dialog').count()) failures.push('Escapeでプレビューが閉じない')

      // 帯域タブ (BS)
      await page.locator('.guide-band-tab', { hasText: 'BS' }).first().click()
      await page.waitForTimeout(800)
      const bs = await page.evaluate(() => ({
        pressed: document.querySelectorAll('.guide-band-tab[aria-pressed="true"]').length,
        first: document.querySelector('.guide-ch-name')?.textContent ?? '',
        cols: document.querySelectorAll('.guide-col').length,
      }))
      if (bs.pressed !== 1) failures.push(`BSタブ aria-pressed=${bs.pressed}`)
      if (!bs.first.startsWith('BS')) failures.push(`BSフィルタが効いていない (先頭=${bs.first})`)
      await page.screenshot({ path: join(output, 'guide-band-bs.png') })
      await page.locator('.guide-band-tab', { hasText: 'すべて' }).first().click()
      await page.waitForTimeout(600)
      // スクロールしても仮想化が保たれるか + jank判定用にセル数を見る
      await page.evaluate(() => { const s = document.querySelector('.guide-scroll'); s.scrollTop = 1200; s.scrollLeft = 900 })
      await page.waitForTimeout(600)
      const scrolled = await page.evaluate(() => ({
        cells: document.querySelectorAll('.guide-cell').length,
        cols: document.querySelectorAll('.guide-col').length,
        headerTop: document.querySelector('.guide-header-row')?.getBoundingClientRect().top,
        scrollTop: document.querySelector('.guide-scroll')?.getBoundingClientRect().top,
        axisLeft: document.querySelector('.guide-timeaxis')?.getBoundingClientRect().left,
        scrollLeft: document.querySelector('.guide-scroll')?.getBoundingClientRect().left,
      }))
      if (scrolled.cols >= 60) failures.push(`スクロール後に列が仮想化されていない ${scrolled.cols}`)
      if (Math.abs(scrolled.headerTop - scrolled.scrollTop) > 2) failures.push(`スクロール後ヘッダーが追従していない (${scrolled.headerTop} vs ${scrolled.scrollTop})`)
      if (Math.abs(scrolled.axisLeft - scrolled.scrollLeft) > 2) failures.push(`スクロール後 時刻軸が追従していない (${scrolled.axisLeft} vs ${scrolled.scrollLeft})`)
      notes.push({ tag: 'scrolled', ...scrolled })
      await page.screenshot({ path: join(output, 'guide-scrolled.png') })
      // 「現在時刻へ」
      await page.locator('.guide-actionbar .guide-chip-button', { hasText: '現在時刻へ' }).click()
      await page.waitForTimeout(600)
      const nowVisible = await page.evaluate(() => {
        const line = document.querySelector('.guide-now-line')?.getBoundingClientRect()
        const s = document.querySelector('.guide-scroll')?.getBoundingClientRect()
        return line && s ? line.top > s.top && line.bottom < s.bottom : false
      })
      if (!nowVisible) failures.push('「現在時刻へ」で現在時刻ラインが画面内に来ない')
    }
    if (consoleErrors.length) failures.push(`${tag}: console error ${JSON.stringify(consoleErrors.slice(0, 3))}`)
    await page.close()
  }
}
await browser.close()
server.close()
console.log(JSON.stringify(notes, null, 1))
if (failures.length) { console.error('\n--- FAILURES ---\n' + failures.join('\n')); process.exit(1) }
console.log('\nすべての番組表チェックに合格')

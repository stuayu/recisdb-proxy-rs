// 実サーバー(実DB)に対する番組表の目視確認用スクリーンショット。
// 使い方: node scripts/local-check.mjs <origin> <YYYY-MM-DD>
import { mkdir } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import { chromium } from '@playwright/test'

const origin = process.argv[2] ?? 'http://127.0.0.1:41080/'
const date = process.argv[3] ?? null
const out = resolve(process.cwd(), 'test-results/local')
await mkdir(out, { recursive: true })

const browser = await chromium.launch({ headless: true })
const report = []
for (const vp of [
  { name: '1920x1080', width: 1920, height: 1080 },
  { name: '1280x720', width: 1280, height: 720 },
  { name: '390', width: 390, height: 844 },
]) {
  const page = await browser.newPage({ viewport: { width: vp.width, height: vp.height } })
  const errors = []
  page.on('console', (m) => { if (m.type() === 'error' && !m.text().startsWith('Failed to load resource')) errors.push(m.text()) })
  page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`))
  page.on('response', (r) => { if (r.status() >= 400 && !r.url().includes('/logos/')) errors.push(`${r.status()} ${r.url()}`) })
  await page.goto(origin, { waitUntil: 'load' })
  await page.evaluate(() => { location.hash = 'guide' })
  await page.waitForTimeout(1500)
  if (date) {
    await page.evaluate((d) => {
      const input = document.querySelector('.guide-date-picker input[type="date"]')
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set
      setter.call(input, d)
      input.dispatchEvent(new Event('input', { bubbles: true }))
      input.dispatchEvent(new Event('change', { bubbles: true }))
    }, date)
    await page.waitForTimeout(2500)
  }
  const m = await page.evaluate(() => {
    const s = document.querySelector('.guide-scroll')
    const r = s?.getBoundingClientRect()
    return {
      pageOverflowX: document.documentElement.scrollWidth - document.documentElement.clientWidth,
      guideShare: r ? Math.round((r.height / window.innerHeight) * 100) : 0,
      domCols: document.querySelectorAll('.guide-col').length,
      domCells: document.querySelectorAll('.guide-cell').length,
      totalCols: (() => {
        const g = document.querySelector('.guide-grid')
        return g ? Math.round(parseFloat(getComputedStyle(g).width)) : 0
      })(),
      chNums: [...document.querySelectorAll('.guide-ch-num')].slice(0, 8).map((e) => e.textContent),
      chNames: [...document.querySelectorAll('.guide-ch-name')].slice(0, 8).map((e) => e.textContent),
      logos: [...document.querySelectorAll('.guide-channel-logo')].length,
      onAir: document.querySelectorAll('.guide-cell-onair').length,
      nowLine: !!document.querySelector('.guide-now-line'),
      empty: document.querySelector('.empty-state')?.textContent?.trim() ?? null,
      err: document.querySelector('.notice.error')?.textContent?.trim() ?? null,
    }
  })
  report.push({ vp: vp.name, ...m, errors })
  await page.screenshot({ path: join(out, `local-${vp.name}.png`) })
  if (vp.name === '1280x720') {
    await page.evaluate(() => { const s = document.querySelector('.guide-scroll'); s.scrollTop = 900 })
    await page.waitForTimeout(600)
    const cell = page.locator('.guide-cell').nth(5)
    if (await cell.count()) {
      await cell.click(); await page.waitForTimeout(700)
      await page.screenshot({ path: join(out, 'local-detail.png') })
      report.push({ vp: 'detail', title: await page.locator('#guide-detail-title').textContent().catch(() => null),
        body: (await page.locator('.guide-detail-dialog .preserve-lines').first().textContent().catch(() => ''))?.slice(0, 90) })
      await page.keyboard.press('Escape'); await page.waitForTimeout(400)
      await page.screenshot({ path: join(out, 'local-selected.png') })
    }
    // ダーク
    await page.evaluate(() => { localStorage.setItem('dashboardTheme', 'dark') })
    await page.reload({ waitUntil: 'load' })
    await page.evaluate(() => { location.hash = 'guide' }); await page.waitForTimeout(1500)
    if (date) {
      await page.evaluate((d) => {
        const input = document.querySelector('.guide-date-picker input[type="date"]')
        const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set
        setter.call(input, d)
        input.dispatchEvent(new Event('input', { bubbles: true }))
        input.dispatchEvent(new Event('change', { bubbles: true }))
      }, date)
      await page.waitForTimeout(2500)
    }
    await page.screenshot({ path: join(out, 'local-dark.png') })
  }
  await page.close()
}
await browser.close()
console.log(JSON.stringify(report, null, 1))

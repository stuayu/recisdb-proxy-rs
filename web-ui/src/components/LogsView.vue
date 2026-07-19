<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { api, downloadApi, type JsonRecord } from '../api'

interface LogEntry {
  seq: number
  timestamp: string
  level: string
  target: string
  message: string
}

interface LogsResponse {
  entries: LogEntry[]
  last_seq: number
  dropped: boolean
}

interface LogFile {
  name: string
  size: number
  modified: string | null
}

const LEVELS = ['ERROR', 'WARN', 'INFO', 'DEBUG', 'TRACE'] as const
type Level = (typeof LEVELS)[number]

const props = defineProps<{ active: boolean }>()

const entries = ref<LogEntry[]>([])
const lastSeq = ref(0)
const error = ref('')
const loading = ref(false)
const paused = ref(false)
const followLatest = ref(true)
const showNewButton = ref(false)

const levelFilter = ref<Level | ''>('')
const targetFilter = ref('')
const queryText = ref('')

const files = ref<LogFile[]>([])
const filesError = ref('')

const logBody = ref<HTMLElement | null>(null)
let pollTimer = 0

const errorCount = computed(() => entries.value.filter((e) => e.level === 'ERROR').length)
const warnCount = computed(() => entries.value.filter((e) => e.level === 'WARN').length)

function buildQuery(afterSeq: number): string {
  const params = new URLSearchParams()
  if (levelFilter.value) params.set('level', levelFilter.value)
  if (targetFilter.value.trim()) params.set('target', targetFilter.value.trim())
  if (queryText.value.trim()) params.set('q', queryText.value.trim())
  if (afterSeq > 0) params.set('after_seq', String(afterSeq))
  params.set('limit', '500')
  return `/logs?${params.toString()}`
}

function isNearBottom(): boolean {
  const el = logBody.value
  if (!el) return true
  return el.scrollHeight - el.scrollTop - el.clientHeight < 48
}

async function scrollToBottom() {
  await nextTick()
  const el = logBody.value
  if (el) el.scrollTop = el.scrollHeight
}

async function fetchLogs(reset: boolean) {
  loading.value = true
  try {
    const data = await api<LogsResponse>(buildQuery(reset ? 0 : lastSeq.value))
    if (reset || data.dropped) {
      entries.value = data.entries
    } else if (data.entries.length) {
      entries.value = [...entries.value, ...data.entries]
      // Keep the buffer from growing without bound in a long-running tab.
      if (entries.value.length > 5000) {
        entries.value = entries.value.slice(entries.value.length - 5000)
      }
    }
    lastSeq.value = data.last_seq
    error.value = ''
    if (data.entries.length && followLatest.value) {
      await scrollToBottom()
    } else if (data.entries.length && !followLatest.value) {
      showNewButton.value = true
    }
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

async function reload() {
  await fetchLogs(true)
  followLatest.value = true
  showNewButton.value = false
  await scrollToBottom()
}

function onScroll() {
  const atBottom = isNearBottom()
  if (atBottom) {
    followLatest.value = true
    showNewButton.value = false
  } else {
    followLatest.value = false
  }
}

function jumpToLatest() {
  followLatest.value = true
  showNewButton.value = false
  void scrollToBottom()
}

function togglePaused() {
  paused.value = !paused.value
}

function startPolling() {
  stopPolling()
  pollTimer = window.setInterval(() => {
    if (paused.value || !props.active) return
    void fetchLogs(false)
  }, 2000)
}

function stopPolling() {
  if (pollTimer) {
    window.clearInterval(pollTimer)
    pollTimer = 0
  }
}

async function loadFiles() {
  try {
    const data = await api<{ files: LogFile[] } | JsonRecord>('/logs/files')
    files.value = Array.isArray((data as { files?: LogFile[] }).files)
      ? (data as { files: LogFile[] }).files
      : []
    filesError.value = ''
  } catch (e) {
    filesError.value = e instanceof Error ? e.message : String(e)
  }
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

async function download(name: string) {
  try {
    await downloadApi(`/logs/files/${encodeURIComponent(name)}`, name)
  } catch (e) {
    filesError.value = e instanceof Error ? e.message : String(e)
  }
}

watch([levelFilter, targetFilter, queryText], () => {
  void reload()
})

watch(
  () => props.active,
  (isActive) => {
    if (isActive) {
      void fetchLogs(entries.value.length === 0)
      startPolling()
    } else {
      stopPolling()
    }
  },
)

onMounted(() => {
  void reload()
  void loadFiles()
  if (props.active) startPolling()
})

onUnmounted(() => {
  stopPolling()
})
</script>

<template>
  <section class="view">
    <div class="view-heading">
      <div>
        <h2>ログ</h2>
        <p>サーバーログをリアルタイムに閲覧します。</p>
      </div>
      <div class="actions">
        <span v-if="errorCount" class="log-badge error" v-text="`ERROR ${errorCount}`" />
        <span v-if="warnCount" class="log-badge warn" v-text="`WARN ${warnCount}`" />
        <button class="button secondary" @click="togglePaused" v-text="paused ? '再開' : '一時停止'" />
        <button class="button" :disabled="loading" @click="reload">再取得</button>
      </div>
    </div>

    <form class="toolbar" @submit.prevent="reload">
      <label class="field">
        <span>レベル（以上）</span>
        <select v-model="levelFilter">
          <option value="">すべて</option>
          <option v-for="lv in LEVELS" :key="lv" :value="lv" v-text="lv" />
        </select>
      </label>
      <label class="field">
        <span>ターゲット絞り込み</span>
        <input v-model="targetFilter" placeholder="例: tuner::pool" />
      </label>
      <label class="search">
        <span>メッセージ検索</span>
        <input v-model="queryText" placeholder="キーワード" />
      </label>
    </form>

    <p v-if="error" class="notice error" role="alert" v-text="error" />

    <div class="log-viewport">
      <div ref="logBody" class="log-body" @scroll="onScroll">
        <p v-if="!entries.length" class="empty-state">ログはまだありません</p>
        <div
          v-for="entry in entries"
          :key="entry.seq"
          class="log-line"
          :class="entry.level.toLowerCase()"
        >
          <span class="log-time" v-text="entry.timestamp" />
          <span class="log-level" v-text="entry.level" />
          <span class="log-target" v-text="entry.target" />
          <span class="log-message" v-text="entry.message" />
        </div>
      </div>
      <button v-if="showNewButton" class="button log-jump" @click="jumpToLatest">最新へ ↓</button>
    </div>

    <details class="log-files">
      <summary>過去のログファイル (<span v-text="files.length" />)</summary>
      <div class="log-files-body">
        <p v-if="filesError" class="notice error" v-text="filesError" />
        <p v-else-if="!files.length" class="empty-state">ログファイルはありません</p>
        <table v-else class="data-table">
          <thead>
            <tr>
              <th>ファイル名</th>
              <th>サイズ</th>
              <th>更新日時</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="file in files" :key="file.name">
              <td v-text="file.name" />
              <td v-text="formatSize(file.size)" />
              <td v-text="file.modified ?? '—'" />
              <td>
                <button class="button small secondary" @click="download(file.name)">
                  ダウンロード
                </button>
              </td>
            </tr>
          </tbody>
        </table>
        <button class="button secondary small" @click="loadFiles">一覧を更新</button>
      </div>
    </details>
  </section>
</template>

<style scoped>
.log-viewport {
  position: relative;
}

.log-body {
  height: 60vh;
  min-height: 320px;
  overflow-y: auto;
  padding: 10px 14px;
  background: #101820;
  color: #e6edf3;
  border: 1px solid var(--border);
  border-radius: 8px;
  font:
    12.5px/1.6 ui-monospace,
    Consolas,
    monospace;
}

.log-line {
  display: flex;
  gap: 10px;
  white-space: pre-wrap;
  word-break: break-word;
  padding: 1px 0;
}

.log-time {
  flex: 0 0 auto;
  color: #8a97a8;
}

.log-level {
  flex: 0 0 52px;
  font-weight: 700;
}

.log-target {
  flex: 0 0 auto;
  max-width: 26ch;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: #9db4d1;
}

.log-message {
  flex: 1 1 auto;
}

.log-line.error {
  color: #ff8f80;
}

.log-line.error .log-level {
  color: #ff5449;
}

.log-line.warn {
  color: #f3d38a;
}

.log-line.warn .log-level {
  color: #e7a62a;
}

.log-line.info .log-level {
  color: #6fb4f0;
}

.log-line.debug,
.log-line.trace {
  color: #7f8b99;
}

.log-jump {
  position: absolute;
  bottom: 16px;
  right: 16px;
}

.log-badge {
  display: inline-flex;
  align-items: center;
  min-height: 44px;
  padding: 0 12px;
  border-radius: 8px;
  font-weight: 700;
}

.log-badge.error {
  color: var(--danger);
  background: #fce9e7;
}

.log-badge.warn {
  color: #8a5a10;
  background: #fbeecb;
}

.log-files {
  margin-top: 16px;
}

.log-files summary {
  min-height: 44px;
  display: flex;
  align-items: center;
  padding: 8px 12px;
  width: fit-content;
  color: var(--text);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  cursor: pointer;
  font-weight: 700;
}

.log-files-body {
  padding: 12px 4px 4px;
  display: grid;
  gap: 10px;
}

.app.dark .log-badge.warn {
  color: #f3d38a;
  background: #4a3a14;
}
</style>

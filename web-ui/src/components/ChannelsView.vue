<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref } from 'vue'
import { api, downloadApi, unwrapArray, type JsonRecord } from '../api'
import PreviewPlayer from './PreviewPlayer.vue'

type Column = {
  key: string
  label: string
  compact?: boolean
}

const columns: Column[] = [
  { key: 'is_enabled', label: '有効', compact: true },
  { key: 'channel_name', label: '名称', compact: true },
  { key: 'nid', label: 'NID', compact: true },
  { key: 'sid', label: 'SID', compact: true },
  { key: 'tsid', label: 'TSID' },
  { key: 'band_type', label: 'バンド' },
  { key: 'terrestrial_region', label: '地域' },
  { key: 'network_name', label: 'ネットワーク' },
  { key: 'bon_driver_path', label: 'BonDriver' },
  { key: 'bon_space', label: 'BonSpace' },
  { key: 'bon_channel', label: 'BonChannel' },
  { key: 'priority', label: '優先度', compact: true },
  { key: 'id', label: 'ID' },
  { key: 'raw_name', label: 'raw名' },
  { key: 'manual_sheet', label: '枝番' },
  { key: 'physical_ch', label: '物理CH' },
  { key: 'remote_control_key', label: 'リモコン' },
  { key: 'service_type', label: 'サービス種別' },
  { key: 'failure_count', label: '失敗回数' },
  { key: 'scan_time', label: 'スキャン日時' },
  { key: 'last_seen', label: '最終確認' },
  { key: 'created_at', label: '登録日時' },
  { key: 'updated_at', label: '更新日時' },
]

const defaultKeys = columns.slice(0, 12).map((column) => column.key)
const rows = ref<JsonRecord[]>([])
const query = ref('')
const error = ref('')
const message = ref('')
const loading = ref(false)
const editing = ref<number | null>(null)
const editMode = ref(false)
const importFile = ref<HTMLInputElement | null>(null)
const selected = ref<number[]>([])
const bulkPriority = ref<number | null>(null)
const bulkEnabled = ref('')
const bulkDelete = ref(false)
const sortKey = ref('channel_name')
const descending = ref(false)
const compactViewport = ref(false)
const customizedColumns = ref(false)
let media: MediaQueryList | null = null

const savedColumns = localStorage.getItem('channelColumns')
function initialColumns(): string[] {
  if (!savedColumns) return defaultKeys
  try {
    const parsed = JSON.parse(savedColumns)
    if (Array.isArray(parsed)) {
      const valid = parsed.filter(
        (key): key is string =>
          typeof key === 'string' && columns.some((column) => column.key === key),
      )
      if (valid.length) return valid
    }
  } catch {
    localStorage.removeItem('channelColumns')
  }
  return defaultKeys
}
const visibleKeys = ref<string[]>(initialColumns())
customizedColumns.value = Boolean(savedColumns)

const form = reactive({
  bon_driver_id: 0,
  nid: 0,
  sid: 0,
  tsid: 0,
  channel_name: '',
  bon_space: null as number | null,
  bon_channel: null as number | null,
  priority: 0,
  is_enabled: true,
})

const filtered = computed(() => {
  const normalized = query.value.trim().toLowerCase()
  if (!normalized) return rows.value
  return rows.value.filter((row) =>
    Object.values(row).some((value) =>
      String(value ?? '')
        .toLowerCase()
        .includes(normalized),
    ),
  )
})

const sorted = computed(() => {
  const key = sortKey.value
  return [...filtered.value].sort((left, right) => {
    const a = left[key]
    const b = right[key]
    const numeric = typeof a === 'number' && typeof b === 'number'
    const compared = numeric
      ? a - b
      : String(a ?? '').localeCompare(String(b ?? ''), 'ja', { numeric: true })
    return descending.value ? -compared : compared
  })
})

const visibleColumns = computed(() => {
  const selectedKeys = new Set(visibleKeys.value)
  return columns.filter(
    (column) =>
      selectedKeys.has(column.key) &&
      (!compactViewport.value || customizedColumns.value || column.compact),
  )
})

const allSelected = computed(
  () =>
    sorted.value.length > 0 && sorted.value.every((row) => selected.value.includes(Number(row.id))),
)

async function load() {
  loading.value = true
  try {
    rows.value = unwrapArray(await api('/channels'), ['channels'])
    selected.value = selected.value.filter((id) => rows.value.some((row) => Number(row.id) === id))
    error.value = ''
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    loading.value = false
  }
}

function reset() {
  editing.value = null
  Object.assign(form, {
    bon_driver_id: 0,
    nid: 0,
    sid: 0,
    tsid: 0,
    channel_name: '',
    bon_space: null,
    bon_channel: null,
    priority: 0,
    is_enabled: true,
  })
}

function edit(row: JsonRecord) {
  editing.value = Number(row.id)
  editMode.value = true
  for (const key of Object.keys(form)) {
    const value = row[key]
    if (value !== undefined) (form as Record<string, unknown>)[key] = value
  }
  requestAnimationFrame(() =>
    document.getElementById('channel-form')?.scrollIntoView({ behavior: 'smooth' }),
  )
}

async function save() {
  try {
    const path = editing.value ? `/channel/${editing.value}` : '/channel'
    await api(path, { method: 'POST', body: JSON.stringify(form) })
    message.value = editing.value ? 'チャンネルを更新しました' : 'チャンネルを登録しました'
    reset()
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}

async function remove(id: unknown) {
  if (!confirm('チャンネルを削除しますか？')) return
  try {
    await api(`/channel/${id}`, { method: 'DELETE' })
    message.value = 'チャンネルを削除しました'
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}

async function toggle(row: JsonRecord) {
  try {
    await api(`/channel/${row.id}/toggle`, {
      method: 'POST',
      body: JSON.stringify({ enabled: !row.is_enabled }),
    })
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}

async function importCsv(event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  if (!file) return
  try {
    const result = await api<JsonRecord>('/channels/import', {
      method: 'POST',
      headers: { 'Content-Type': 'text/csv; charset=utf-8' },
      body: await file.text(),
    })
    const inserted = Number(result.inserted ?? 0)
    const updated = Number(result.updated ?? 0)
    const errors = Array.isArray(result.errors) ? result.errors : []
    message.value = `${inserted}件登録、${updated}件更新${errors.length ? `、${errors.length}件エラー` : ''}`
    if (errors.length) error.value = errors.map(String).join('\n')
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    input.value = ''
  }
}

async function exportCsv() {
  try {
    await downloadApi('/channels/export', 'channels.csv')
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}

async function applyBulk() {
  if (!selected.value.length) return
  if (bulkDelete.value && !confirm(`${selected.value.length}件を削除しますか？`)) return
  const payload = selected.value.map((id) => ({
    id,
    priority: bulkPriority.value ?? undefined,
    is_enabled: bulkEnabled.value === '' ? undefined : bulkEnabled.value === 'true',
    deleted: bulkDelete.value || undefined,
  }))
  try {
    await api('/channels/batch', { method: 'POST', body: JSON.stringify(payload) })
    message.value = `${selected.value.length}件を一括更新しました`
    selected.value = []
    bulkPriority.value = null
    bulkEnabled.value = ''
    bulkDelete.value = false
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}

function toggleAll() {
  if (allSelected.value) {
    const visibleIds = new Set(sorted.value.map((row) => Number(row.id)))
    selected.value = selected.value.filter((id) => !visibleIds.has(id))
  } else {
    selected.value = Array.from(
      new Set([...selected.value, ...sorted.value.map((row) => Number(row.id))]),
    )
  }
}

function sort(column: string) {
  if (sortKey.value === column) descending.value = !descending.value
  else {
    sortKey.value = column
    descending.value = false
  }
}

function setColumn(key: string, checked: boolean) {
  const next = new Set(visibleKeys.value)
  if (checked) next.add(key)
  else next.delete(key)
  visibleKeys.value = columns.map((column) => column.key).filter((column) => next.has(column))
  customizedColumns.value = true
  localStorage.setItem('channelColumns', JSON.stringify(visibleKeys.value))
}

function resetColumns() {
  visibleKeys.value = defaultKeys
  customizedColumns.value = false
  localStorage.removeItem('channelColumns')
}

function display(row: JsonRecord, key: string): string {
  const value = row[key]
  if (value === null || value === undefined || value === '') return '—'
  if (['scan_time', 'last_seen', 'created_at', 'updated_at'].includes(key)) {
    const seconds = Number(value)
    return seconds > 0 ? new Date(seconds * 1000).toLocaleString('ja-JP') : '—'
  }
  if (key === 'service_type') {
    const number = Number(value)
    const labels: Record<number, string> = {
      1: 'TV',
      2: '音声',
      161: '臨時',
      165: 'プロモ',
      192: 'データ',
    }
    return labels[number] ?? `0x${number.toString(16).toUpperCase()}`
  }
  if (Array.isArray(value)) return value.map(String).join(', ')
  return typeof value === 'object' ? JSON.stringify(value) : String(value)
}

onMounted(() => {
  media = window.matchMedia('(max-width: 1100px)')
  const update = () => (compactViewport.value = media?.matches ?? false)
  update()
  media.addEventListener('change', update)
  void load()
})

onUnmounted(() => media?.removeEventListener('change', () => undefined))
</script>

<template>
  <section class="view">
    <div class="view-heading">
      <div>
        <h2>チャンネル</h2>
        <p>検索・ソート・表示列・登録・編集・一括更新・CSV入出力</p>
      </div>
      <div class="actions">
        <input
          ref="importFile"
          class="visually-hidden"
          type="file"
          accept=".csv,text/csv"
          @change="importCsv"
        />
        <button class="button secondary" @click="importFile?.click()">CSV読込</button>
        <button class="button secondary" @click="exportCsv">CSV出力</button>
        <button class="button secondary" @click="editMode = !editMode">
          editMode ? '編集を閉じる' : '編集モード'
        </button>
        <button
          class="button"
          @click="
            editMode = true; reset()"
        >
          新規
        </button>
      </div>
    </div>

    <div class="channel-toolbar">
      <label class="search">
        <span>絞り込み</span>
        <input v-model="query" type="search" placeholder="名称、NID、SID、地域など" />
      </label>
      <label class="mobile-sort">
        <span>並び替え</span>
        <select v-model="sortKey">
          <option
            v-for="column in columns"
            :key="column.key"
            :value="column.key"
            v-text="column.label"
          ></option>
        </select>
        <button
          class="button small secondary"
          @click="descending = !descending"
          v-text="descending ? '降順' : '昇順'"
        ></button>
      </label>
      <details class="column-picker">
        <summary><span v-text="`表示列を調整（${visibleColumns.length}列）`"></span></summary>
        <div class="column-options">
          <label v-for="column in columns" :key="column.key" class="check compact-check">
            <input
              type="checkbox"
              :checked="visibleKeys.includes(column.key)"
              @change="setColumn(column.key, ($event.target as HTMLInputElement).checked)"
            />
            <span v-text="column.label"></span>
          </label>
        </div>
        <button class="button small secondary" @click="resetColumns">既定に戻す</button>
      </details>
    </div>

    <p v-if="message" class="notice success" aria-live="polite" v-text="message"></p>
    <p v-if="error" class="notice error preserve-lines" role="alert" v-text="error"></p>

    <div v-if="editMode" class="bulk-bar">
      <strong v-text="`${selected.length}件を選択`"></strong>
      <label>優先度 <input v-model.number="bulkPriority" type="number" /></label>
      <label>
        有効
        <select v-model="bulkEnabled">
          <option value="">変更なし</option>
          <option value="true">有効</option>
          <option value="false">無効</option>
        </select>
      </label>
      <label class="check compact-check">
        <input v-model="bulkDelete" type="checkbox" />削除
      </label>
      <button class="button small" :disabled="!selected.length" @click="applyBulk">一括更新</button>
    </div>

    <div :class="['split', 'wide', { 'without-panel': !editMode }]">
      <div class="table-region" role="region" aria-label="チャンネル一覧" tabindex="0">
        <table class="data-table channel-table">
          <thead>
            <tr>
              <th v-if="editMode">
                <input
                  type="checkbox"
                  :checked="allSelected"
                  aria-label="表示中のチャンネルをすべて選択"
                  @change="toggleAll"
                />
              </th>
              <th v-for="column in visibleColumns" :key="column.key">
                <button class="sort-button" @click="sort(column.key)">
                  <span v-text="column.label"></span>
                  <span
                    aria-hidden="true"
                    v-text="sortKey === column.key ? (descending ? '↓' : '↑') : '↕'"
                  ></span>
                </button>
              </th>
              <th v-if="editMode">操作</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="row in sorted" :key="String(row.id)">
              <td v-if="editMode" data-label="選択">
                <input
                  v-model="selected"
                  type="checkbox"
                  :value="Number(row.id)"
                  :aria-label="`${String(row.channel_name ?? row.id)}を選択`"
                />
              </td>
              <td v-for="column in visibleColumns" :key="column.key" :data-label="column.label">
                <button
                  v-if="column.key === 'is_enabled'"
                  class="status-button"
                  :aria-label="`${String(row.channel_name ?? row.id)}を${row.is_enabled ? '無効' : '有効'}にする`"
                  @click="toggle(row)"
                  v-text="row.is_enabled ? '有効' : '無効'"
                ></button>
                <span v-else v-text="display(row, column.key)"></span>
              </td>
              <td v-if="editMode" data-label="操作">
                <div class="actions">
                  <button class="button small secondary" @click="edit(row)">編集</button>
                  <button class="button small danger" @click="remove(row.id)">削除</button>
                </div>
              </td>
            </tr>
          </tbody>
        </table>
        <p v-if="!loading && !sorted.length" class="empty-state">該当するチャンネルはありません</p>
      </div>

      <form v-if="editMode" id="channel-form" class="panel" @submit.prevent="save">
        <h3>editing ? 'チャンネル編集' : 'チャンネル登録'</h3>
        <label class="field">
          <span>BonDriver ID</span>
          <input v-model.number="form.bon_driver_id" type="number" min="1" required />
        </label>
        <div class="form-grid">
          <label class="field"
            ><span>NID</span
            ><input v-model.number="form.nid" type="number" min="0" max="65535" required
          /></label>
          <label class="field"
            ><span>SID</span
            ><input v-model.number="form.sid" type="number" min="0" max="65535" required
          /></label>
          <label class="field"
            ><span>TSID</span
            ><input v-model.number="form.tsid" type="number" min="0" max="65535" required
          /></label>
        </div>
        <label class="field"><span>名称</span><input v-model="form.channel_name" /></label>
        <div class="form-grid two-columns">
          <label class="field"
            ><span>BonSpace</span><input v-model.number="form.bon_space" type="number" min="0"
          /></label>
          <label class="field"
            ><span>BonChannel</span><input v-model.number="form.bon_channel" type="number" min="0"
          /></label>
        </div>
        <label class="field"
          ><span>優先度</span><input v-model.number="form.priority" type="number"
        /></label>
        <label class="check"><input v-model="form.is_enabled" type="checkbox" />有効</label>
        <div class="actions">
          <button class="button" type="submit">保存</button>
          <button class="button secondary" type="button" @click="reset">リセット</button>
        </div>
      </form>
    </div>

    <PreviewPlayer></PreviewPlayer>
  </section>
</template>

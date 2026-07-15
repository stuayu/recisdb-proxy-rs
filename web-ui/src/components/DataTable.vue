<script setup lang="ts">
import { computed, ref } from 'vue'
import type { JsonRecord } from '../api'
import { columnLabels } from '../labels'
import { useColumnVisibility, type ColumnDef } from '../columns'
import ColumnPicker from './ColumnPicker.vue'
const props = withDefaults(
  defineProps<{
    rows: JsonRecord[]
    columns?: string[]
    labels?: Record<string, string>
    storageKey?: string
    empty?: string
  }>(),
  { columns: () => [], labels: undefined, storageKey: '', empty: 'データがありません' },
)
function label(col: string) {
  return props.labels?.[col] ?? columnLabels[col] ?? col
}
const sortKey = ref('')
const descending = ref(false)
// 選択可能な全列: 明示指定列を先頭に、行データに現れる残りのキーを後ろに足す。
const allColumns = computed<ColumnDef[]>(() => {
  const fromRows = Array.from(new Set(props.rows.flatMap(Object.keys)))
  const base = props.columns?.length
    ? [...props.columns, ...fromRows.filter((key) => !props.columns.includes(key))]
    : fromRows
  return base.map((key) => ({ key, label: label(key) }))
})
// 既定表示: 明示指定列があればそれ、なければ先頭12列。
const defaultKeys = () =>
  props.columns?.length
    ? props.columns
    : allColumns.value.slice(0, 12).map((column) => column.key)
const { visibleKeys, setColumn, resetColumns } = useColumnVisibility(
  props.storageKey,
  () => allColumns.value,
  defaultKeys,
)
const columns = computed(() => visibleKeys.value)
const sorted = computed(() =>
  !sortKey.value
    ? props.rows
    : [...props.rows].sort(
        (a, b) =>
          String(a[sortKey.value] ?? '').localeCompare(String(b[sortKey.value] ?? ''), 'ja', {
            numeric: true,
          }) * (descending.value ? -1 : 1),
      ),
)
function sort(key: string) {
  if (sortKey.value === key) descending.value = !descending.value
  else {
    sortKey.value = key
    descending.value = false
  }
}
const timeKeyPattern = /(_at$|_time$|_seen$|^start(ed)?(_at)?$|^end(ed)?(_at)?$|timestamp)/i
// 数値の桁数規則: バイト量=単位換算で小数1桁 / Mbps=小数2桁 /
// 信号レベル=小数1桁 / 整数カウンタ=3桁区切り / その他の小数=最大2桁
function formatBytes(value: number) {
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let scaled = Math.max(0, value)
  let unit = 0
  while (scaled >= 1024 && unit < units.length - 1) {
    scaled /= 1024
    unit += 1
  }
  return unit === 0 ? `${Math.floor(scaled)} B` : `${scaled.toFixed(1)} ${units[unit]}`
}
function display(value: unknown, col?: string) {
  if (value == null || value === '') return '—'
  if (typeof value === 'boolean') {
    if (col && /success/i.test(col)) return value ? '成功' : '失敗'
    return value ? 'はい' : 'いいえ'
  }
  if (typeof value === 'number' && col) {
    // Epoch seconds between 2001-09 and 5138-11: format as local datetime.
    if (timeKeyPattern.test(col) && value > 1_000_000_000 && value < 100_000_000_000) {
      return new Date(value * 1000).toLocaleString('ja-JP')
    }
    if (/duration|_secs$|_seconds$/i.test(col)) {
      const total = Math.max(0, Math.floor(value))
      const hours = Math.floor(total / 3600)
      const minutes = Math.floor((total % 3600) / 60)
      const seconds = total % 60
      if (hours) return `${hours}時間${minutes}分${seconds}秒`
      if (minutes) return `${minutes}分${seconds}秒`
      return `${seconds}秒`
    }
    if (/bytes/i.test(col)) return formatBytes(value)
    if (/mbps|bitrate/i.test(col)) return `${value.toFixed(2)} Mbps`
    if (/signal|_level$|_db$/i.test(col)) return value.toFixed(1)
    if (Number.isInteger(value)) return value.toLocaleString('ja-JP')
    return value.toLocaleString('ja-JP', { maximumFractionDigits: 2 })
  }
  return typeof value === 'object' ? JSON.stringify(value) : String(value)
}
function marker(col: string) {
  return sortKey.value === col ? (descending.value ? '↓' : '↑') : '↕'
}
</script>
<template>
  <ColumnPicker
    v-if="rows.length"
    :columns="allColumns"
    :visible-keys="visibleKeys"
    @set="setColumn"
    @reset="resetColumns"
  />
  <div class="table-region" role="region" aria-label="データ一覧" tabindex="0">
    <table v-if="rows.length" class="data-table">
      <thead>
        <tr>
          <th v-for="col in columns" :key="col">
            <button class="sort-button" @click="sort(col)">
              <span v-text="label(col)" /><span aria-hidden="true" v-text="marker(col)" />
            </button>
          </th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="(row, index) in sorted" :key="String(row.id ?? row.session_id ?? index)">
          <td
            v-for="col in columns"
            :key="col"
            :data-label="label(col)"
            v-text="display(row[col], col)"
          />
        </tr>
      </tbody>
    </table>
    <p v-else class="empty-state" v-text="empty" />
  </div>
</template>

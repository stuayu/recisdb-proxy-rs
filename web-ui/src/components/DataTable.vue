<script setup lang="ts">
import { computed, ref } from 'vue'
import type { JsonRecord } from '../api'
const props = withDefaults(
  defineProps<{ rows: JsonRecord[]; columns?: string[]; empty?: string }>(),
  { empty: 'データがありません' },
)
const sortKey = ref('')
const descending = ref(false)
const columns = computed(() =>
  props.columns?.length
    ? props.columns
    : Array.from(new Set(props.rows.flatMap(Object.keys))).slice(0, 12),
)
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
function display(value: unknown) {
  if (value == null || value === '') return '—'
  return typeof value === 'object' ? JSON.stringify(value) : String(value)
}
function marker(col: string) {
  return sortKey.value === col ? (descending.value ? '↓' : '↑') : '↕'
}
</script>
<template>
  <div class="table-region" role="region" aria-label="データ一覧" tabindex="0">
    <table v-if="rows.length" class="data-table">
      <thead>
        <tr>
          <th v-for="col in columns" :key="col">
            <button class="sort-button" @click="sort(col)">
              <span v-text="col"></span><span aria-hidden="true" v-text="marker(col)"></span>
            </button>
          </th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="(row, index) in sorted" :key="String(row.id ?? row.session_id ?? index)">
          <td v-for="col in columns" :key="col" :data-label="col" v-text="display(row[col])"></td>
        </tr>
      </tbody>
    </table>
    <p v-else class="empty-state" v-text="empty"></p>
  </div>
</template>

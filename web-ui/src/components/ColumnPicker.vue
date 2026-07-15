<script setup lang="ts">
import type { ColumnDef } from '../columns'
defineProps<{ columns: ColumnDef[]; visibleKeys: string[] }>()
const emit = defineEmits<{ set: [key: string, checked: boolean]; reset: [] }>()
</script>
<template>
  <details class="column-picker">
    <summary>
      <span v-text="`表示列を調整（${visibleKeys.length}/${columns.length}列）`" />
    </summary>
    <div class="column-options">
      <label v-for="column in columns" :key="column.key" class="check compact-check">
        <input
          type="checkbox"
          :checked="visibleKeys.includes(column.key)"
          @change="emit('set', column.key, ($event.target as HTMLInputElement).checked)"
        />
        <span v-text="column.label" />
      </label>
    </div>
    <button class="button small secondary" type="button" @click="emit('reset')">既定に戻す</button>
  </details>
</template>

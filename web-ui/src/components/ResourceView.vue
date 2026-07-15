<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { api, unwrapArray, type JsonRecord } from '../api'
import DataTable from './DataTable.vue'
const props = defineProps<{
  title: string
  endpoint: string
  keys: string[]
  columns?: string[]
  storageKey?: string
  description?: string
}>()
const rows = ref<JsonRecord[]>([])
const loading = ref(false)
const error = ref('')
async function load() {
  loading.value = true
  try {
    rows.value = unwrapArray(await api<unknown>(props.endpoint), props.keys)
    error.value = ''
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}
onMounted(load)
</script>
<template>
  <section class="view">
    <div class="view-heading">
      <div>
        <h2 v-text="title" />
        <p v-if="description" v-text="description" />
      </div>
      <button class="button" :disabled="loading" @click="load">更新</button>
    </div>
    <p v-if="error" class="notice error" role="alert" v-text="error" />
    <DataTable :rows="rows" :columns="columns" :storage-key="storageKey" />
  </section>
</template>

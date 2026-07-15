<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, unwrapArray, type JsonRecord } from '../api'
import DataTable from './DataTable.vue'

const address = ref('')
const rows = ref<JsonRecord[]>([])
const loading = ref(false)
const error = ref('')
const endpoint = computed(() => {
  const query = address.value.trim()
  return query ? `/session-history?client_address=${encodeURIComponent(query)}` : '/session-history'
})
async function load() {
  loading.value = true
  try {
    rows.value = unwrapArray(await api<unknown>(endpoint.value), ['history', 'sessions', 'data'])
    error.value = ''
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    loading.value = false
  }
}
function clear() { address.value = ''; void load() }
onMounted(load)
</script>
<template>
  <section class="view">
    <div class="view-heading"><div><h2>セッション履歴</h2><p>過去の接続・切断・視聴セッションを確認します。</p></div><button class="button" :disabled="loading" @click="load">更新</button></div>
    <form class="toolbar" @submit.prevent="load">
      <label class="field"><span>クライアントアドレスで絞り込み</span><input v-model="address" placeholder="例: 192.168.1.10" /></label>
      <div class="actions"><button class="button" type="submit">検索</button><button class="button secondary" type="button" @click="clear">解除</button></div>
    </form>
    <p v-if="error" class="notice error" role="alert" v-text="error"></p>
    <DataTable :rows="rows" empty="該当するセッション履歴はありません"></DataTable>
  </section>
</template>

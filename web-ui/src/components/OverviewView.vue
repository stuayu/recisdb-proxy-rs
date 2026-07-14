<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, type JsonRecord } from '../api'
import { useDashboardStore } from '../stores/dashboard'
import MetricsChart from './MetricsChart.vue'
const store = useDashboardStore()
const selectedSession = ref<string | number>('')
const cards = computed(() => [
  ['アクティブチューナー', store.stats.active_tuners ?? '—'],
  ['接続クライアント', store.stats.active_sessions ?? store.clients.length],
  ['総セッション', store.stats.total_sessions ?? '—'],
  ['登録チャンネル', store.stats.total_channels ?? '—'],
])
async function disconnect(row: JsonRecord) {
  if (!confirm('このクライアントを切断しますか？')) return
  await api(`/client/${row.session_id}/disconnect`, { method: 'POST' })
  if (selectedSession.value === row.session_id) selectedSession.value = ''
  await store.refresh()
}
async function setPriority(row: JsonRecord, event: Event) {
  const value = (event.target as HTMLSelectElement).value
  await api(`/client/${row.session_id}/controls`, {
    method: 'POST',
    body: JSON.stringify({ override_priority: value === '' ? null : Number(value) }),
  })
  await store.refresh()
}
async function setExclusive(row: JsonRecord, event: Event) {
  const value = (event.target as HTMLSelectElement).value
  await api(`/client/${row.session_id}/controls`, {
    method: 'POST',
    body: JSON.stringify({ override_exclusive: value === '' ? null : value === 'true' }),
  })
  await store.refresh()
}
onMounted(() => store.start())
onUnmounted(() => store.stop())
</script>
<template>
  <section class="view">
    <div class="view-heading">
      <div>
        <h2>概要</h2>
        <p>チューナーとクライアントの現在状態</p>
      </div>
      <button class="button" :disabled="store.loading" @click="store.refresh">更新</button>
    </div>
    <p v-if="store.error" class="notice error" role="alert" v-text="store.error"></p>
    <div class="stat-grid">
      <article v-for="card in cards" :key="String(card[0])" class="stat-card">
        <span v-text="card[0]"></span><strong v-text="card[1]"></strong>
      </article>
    </div>
    <h3>接続中のクライアント</h3>
    <div class="table-region">
      <table v-if="store.clients.length" class="data-table">
        <thead>
          <tr>
            <th>クライアント</th>
            <th>チャンネル</th>
            <th>信号</th>
            <th>優先度</th>
            <th>排他</th>
            <th>操作</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="row in store.clients" :key="String(row.session_id)">
            <td
              data-label="クライアント"
              v-text="String(row.host ?? row.address ?? row.session_id)"
            ></td>
            <td data-label="チャンネル" v-text="String(row.channel_name ?? '—')"></td>
            <td
              data-label="信号"
              v-text="row.signal_level == null ? '—' : `${String(row.signal_level)} dB`"
            ></td>
            <td data-label="優先度">
              <select :value="row.override_priority ?? ''" @change="setPriority(row, $event)">
                <option value="">自動</option>
                <option v-for="n in [1, 2, 3, 4, 5]" :key="n" :value="n" v-text="n"></option>
              </select>
            </td>
            <td data-label="排他">
              <select
                :value="row.override_exclusive == null ? '' : String(row.override_exclusive)"
                @change="setExclusive(row, $event)"
              >
                <option value="">自動</option>
                <option value="true">有効</option>
                <option value="false">無効</option>
              </select>
            </td>
            <td data-label="操作">
              <div class="actions">
                <button
                  class="button small secondary"
                  @click="selectedSession = String(row.session_id)"
                >
                  グラフ</button
                ><button class="button small danger" @click="disconnect(row)">切断</button>
              </div>
            </td>
          </tr>
        </tbody>
      </table>
      <p v-else class="empty-state">接続中のクライアントはありません</p>
    </div>
    <MetricsChart v-if="selectedSession" :session-id="selectedSession"></MetricsChart>
  </section>
</template>

<script setup lang="ts">
import NodeHealthSummary from './NodeHealthSummary.vue'
import NodeAdvancedSettings from './NodeAdvancedSettings.vue'
import type { NodeEntry, ProbeResponse } from './types'
import { ref } from 'vue'
defineProps<{
  entry: NodeEntry
  probe?: ProbeResponse
  probing: boolean
}>()
const emit = defineEmits<{ probe: []; edit: []; toggle: []; remove: []; topology: []; saved: [] }>()
const advancedOpen = ref(false)
function selectedEndpoint(probe: ProbeResponse | undefined) {
  return (
    probe?.paths.find((path) => path.id === probe.selected.view)?.endpoint.kind ||
    '利用可能な経路なし'
  )
}
</script>
<template>
  <article class="node-card">
    <div class="head">
      <div>
        <h3>
          {{ entry.node.display_name }} <small>{{ entry.node.site_name || '' }}</small>
        </h3>
        <span :class="['state', entry.paired && entry.node.enabled ? 'good' : 'warn']"
          >●
          {{
            entry.paired && entry.node.enabled
              ? '接続中'
              : entry.paired
                ? '停止中'
                : '接続設定が未完了'
          }}</span
        >
      </div>
      <span class="routes"
        >受信可能 {{ entry.routable_routes }}/{{ entry.total_routes }}チャンネル</span
      >
    </div>
    <p v-if="!entry.routable_routes" class="notice warning" role="status">
      ⚠ 受信可能なチャンネルがありません。<button class="link" @click="emit('probe')">
        原因を確認
      </button>
    </p>
    <NodeHealthSummary :probe="probe" />
    <p v-if="probe" class="muted">現在使用: {{ selectedEndpoint(probe) }}</p>
    <details v-if="probe" class="paths">
      <summary>経路の詳細</summary>
      <div v-for="path in probe.paths" :key="path.id" class="path">
        <span
          >{{ path.endpoint.kind }}
          <b :class="path.endpoint.record_allowed ? 'good' : 'warn'">{{
            path.endpoint.record_allowed
              ? '● 録画利用可'
              : '🟡 視聴には使用できます／🚫 録画には使用しません'
          }}</b></span
        ><small
          >応答
          {{
            Number.isFinite(path.health.rtt_p95_ms)
              ? `${path.health.rtt_p95_ms.toFixed(1)} ms`
              : '—'
          }}・帯域
          {{
            path.health.throughput_down_p10_bps > 0
              ? `${(path.health.throughput_down_p10_bps / 1000000).toFixed(1)} Mbps`
              : '—'
          }}</small
        >
      </div>
    </details>
    <NodeAdvancedSettings :entry="entry" :open="advancedOpen" @saved="emit('saved')" />
    <div class="actions">
      <button class="button" :disabled="probing || !entry.paired" @click="emit('probe')">
        {{ probing ? 'テスト中…' : '通信テスト' }}</button
      ><button class="button secondary" @click="emit('topology')">構成図</button
      ><button class="button secondary" @click="advancedOpen = true">設定</button
      ><button class="button secondary" @click="emit('toggle')">
        {{ entry.node.enabled ? '停止' : '有効化' }}</button
      ><button class="button secondary" @click="emit('remove')">削除</button>
    </div>
  </article>
</template>
<style scoped>
.node-card {
  display: grid;
  gap: 0.8rem;
  padding: 1rem;
  border: 1px solid var(--border, #d9dee7);
  border-radius: 12px;
  background: var(--surface, rgb(255 255 255 / 4%));
}

.head {
  display: flex;
  justify-content: space-between;
  gap: 1rem;
  align-items: start;
  flex-wrap: wrap;
}

.head h3 {
  margin: 0 0 0.3rem;
}

.head small {
  font-weight: 400;
  opacity: 0.7;
}

.state,
.routes {
  font-size: 0.9rem;
}

.good {
  color: #15803d;
}

.warn {
  color: #a16207;
}

.actions {
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem;
}

.link {
  border: 0;
  background: none;
  color: inherit;
  text-decoration: underline;
  cursor: pointer;
}

.notice.warning {
  margin: 0;
  padding: 0.6rem;
  background: rgb(245 158 11 / 12%);
}
</style>

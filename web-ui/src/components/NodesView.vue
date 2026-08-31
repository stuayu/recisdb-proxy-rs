<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api } from '../api'
import NodeCard from './nodes/NodeCard.vue'
import NodeHelpDrawer from './nodes/NodeHelpDrawer.vue'
import NodeSetupWizard from './nodes/NodeSetupWizard.vue'
import NodeTopologyPreview from './nodes/NodeTopologyPreview.vue'
import NodeAdvancedSettings from './nodes/NodeAdvancedSettings.vue'
import RouteAreaEditor from './nodes/RouteAreaEditor.vue'
import { useNodes } from '../composables/useNodes'
import type { NodeEntry } from './nodes/types'
const { data, loading, error, message, probes, probing, load, probe } = useNodes()
const wizard = ref(false)
const help = ref(false)
const topology = ref<NodeEntry | null>(null)
const localName = ref('')
const savingLocal = ref(false)
const nodes = computed(() => data.value?.nodes ?? [])
function setupAction(action: string | null) {
  if (action === 'wizard' || action === 'probe') wizard.value = true
  if (action === 'area') document.querySelector('.area-editor')?.scrollIntoView({ behavior: 'smooth' })
  if (action === 'settings') document.querySelector('details.advanced')?.setAttribute('open', '')
}
async function saveLocal() {
  if (!localName.value.trim()) return
  savingLocal.value = true
  try {
    const result = await api<{ local: { node_id: string; display_name: string } }>('/nodes/local', {
      method: 'POST',
      body: JSON.stringify({ display_name: localName.value.trim() }),
    })
    if (data.value) data.value.local = result.local
  } finally {
    savingLocal.value = false
  }
}
async function toggle(entry: NodeEntry) {
  await api(`/nodes/${encodeURIComponent(entry.node.node_id)}/state`, {
    method: 'POST',
    body: JSON.stringify({ enabled: !entry.node.enabled }),
  })
  await load()
}
async function remove(entry: NodeEntry) {
  if (!window.confirm(`${entry.node.display_name} を削除しますか？`)) return
  await api(`/nodes/${encodeURIComponent(entry.node.node_id)}`, { method: 'DELETE' })
  await load()
}
function edit() {
  help.value = true
}
function finishWizard() {
  wizard.value = false
  void load()
}
onMounted(async () => {
  await load()
  localName.value = data.value?.local.display_name || ''
})
</script>
<template>
  <section class="nodes-view">
    <header class="heading">
      <div>
        <h2>分散ノード</h2>
        <p>別のPC・拠点にあるチューナーを利用できます。</p>
      </div>
      <div class="heading-actions">
        <button class="button secondary" aria-label="設定ガイドを開く" @click="help = true">
          ？ 設定ガイド</button
        ><button class="button secondary" :disabled="loading" @click="load">再読み込み</button>
      </div>
    </header>
    <p v-if="error" class="notice error" role="alert">
      接続設定を確認できません。詳細: {{ error }}
    </p>
    <p v-if="message" class="notice" aria-live="polite">{{ message }}</p>
    <section class="local card">
      <div>
        <h3>このPC</h3>
        <span class="good">● 正常</span>
      </div>
      <label>表示名<input v-model="localName" autocomplete="off" /></label
      ><button class="button secondary" :disabled="savingLocal" @click="saveLocal">保存</button>
    </section>
    <section class="card setup">
      <h3>セットアップ状況</h3>
      <p
        v-for="item in data?.setup_status"
        :key="item.id"
        :class="item.state === 'done' ? 'good' : item.state === 'warn' ? 'warn' : ''"
      >
        {{ item.state === 'done' ? '✓' : item.state === 'warn' ? '!' : '○' }} {{ item.label }}
        <button v-if="item.action" class="link" @click="setupAction(item.action)">確認</button>
      </p>
    </section>
    <RouteAreaEditor v-if="data" :groups="data.route_groups" :nodes="nodes" @changed="load" />
    <section v-if="!loading && !nodes.length" class="empty card">
      <h3>まだ別のPCは接続されていません</h3>
      <p>
        東京のPCから地方局を視聴、別PCの空いているチューナー利用、故障時の別拠点切り替えができます。
      </p>
      <button class="button" @click="wizard = true">＋ 最初のPCを追加</button>
      <NodeAdvancedSettings @saved="load" />
    </section>
    <section v-else class="node-list">
      <div class="list-heading">
        <h3>接続している拠点</h3>
        <button class="button" @click="wizard = true">＋ 別のPC・拠点を追加</button>
      </div>
      <NodeCard
        v-for="entry in nodes"
        :key="entry.node.node_id"
        :entry="entry"
        :probe="probes[entry.node.node_id]"
        :probing="probing === entry.node.node_id"
        @probe="probe(entry)"
        @edit="edit"
        @toggle="toggle(entry)"
        @remove="remove(entry)"
        @topology="topology = entry"
        @saved="load"
      />
    </section>
    <div v-if="topology && data" class="topology-dialog" role="dialog" aria-modal="true">
      <button class="button secondary" @click="topology = null">閉じる</button
      ><NodeTopologyPreview :topology="data.topology" :focus-node-id="topology.node.node_id" />
    </div>
    <NodeSetupWizard
      v-if="wizard"
      @close="wizard = false"
      @complete="finishWizard"
    /><NodeHelpDrawer :open="help" @close="help = false" />
  </section>
</template>
<style scoped>
.nodes-view {
  display: grid;
  gap: 1rem;
}

.heading,
.heading-actions,
.list-heading,
.local {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
  flex-wrap: wrap;
}

.heading h2,
.card h3,
.list-heading h3 {
  margin: 0;
}

.heading p {
  margin: 0.3rem 0;
  color: var(--text-muted, #6b7280);
}

.card {
  padding: 1rem;
  border: 1px solid var(--border, #d9dee7);
  border-radius: 12px;
  background: var(--surface, rgb(255 255 255 / 4%));
}

.local label {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  font-weight: 600;
}

.local input {
  padding: 0.5rem;
}

.setup p {
  margin: 0.4rem 0;
}

.good {
  color: #15803d;
}

.warn {
  color: #a16207;
}

.empty {
  text-align: center;
  padding: 2rem;
}

.node-list {
  display: grid;
  gap: 0.8rem;
}

.topology-dialog {
  position: fixed;
  inset: 0;
  z-index: 15;
  display: grid;
  place-content: center;
  gap: 1rem;
  padding: 1rem;
  background: #0008;
}

.topology-dialog > * {
  max-width: min(40rem, 90vw);
  background: var(--surface, #fff);
}

@media (max-width: 700px) {
  .heading-actions,
  .list-heading {
    width: 100%;
  }

  .heading-actions .button,
  .list-heading .button {
    flex: 1;
  }

  .local label {
    width: 100%;
  }

  .local input {
    flex: 1;
    min-width: 0;
  }
}
</style>

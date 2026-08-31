<script setup lang="ts">
import { ref } from 'vue'
import { api } from '../../api'
import type { NodeEntry, RouteGroup } from './types'

const props = defineProps<{ groups: RouteGroup[]; nodes: NodeEntry[] }>()
const emit = defineEmits<{ changed: [] }>()
const selected = ref<number | null>(null)
const name = ref('')
const nodeId = ref('')
const preference = ref('auto')
const saving = ref(false)
const error = ref('')
const weights = { auto: 100, priority: 200, backup: 50 }
const selectedGroup = () => props.groups.find((group) => group.id === selected.value)

function selectGroup(group: RouteGroup) {
  selected.value = group.id
  name.value = group.name
}
async function saveGroup() {
  if (!name.value.trim()) {
    error.value = '受信エリア名を入力してください。'
    return
  }
  saving.value = true
  error.value = ''
  try {
    const result = await api<{ id: number }>('/node-route-groups', {
      method: 'POST',
      body: JSON.stringify({ id: selected.value, name: name.value.trim() }),
    })
    if (nodeId.value)
      await api('/node-route-groups/member', {
        method: 'POST',
        body: JSON.stringify({
          name: name.value.trim(),
          node_id: nodeId.value,
          weight: weights[preference.value as keyof typeof weights],
        }),
      })
    selected.value = result.id
    emit('changed')
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    saving.value = false
  }
}
async function removeGroup() {
  if (selected.value === null || !window.confirm('この受信エリアを削除しますか？')) return
  await api(`/node-route-groups/${selected.value}`, { method: 'DELETE' })
  selected.value = null
  name.value = ''
  emit('changed')
}
async function removeMember(member: { node_id: string }) {
  if (selected.value === null) return
  await api('/node-route-groups/member', {
    method: 'DELETE',
    body: JSON.stringify({ group_id: selected.value, node_id: member.node_id }),
  })
  emit('changed')
}
function reset() {
  selected.value = null
  name.value = ''
  nodeId.value = ''
}
</script>
<template>
  <section class="area-editor card">
    <div class="area-heading">
      <div>
        <h3>受信エリア</h3>
        <p class="muted">エリア内から状態の良い受信機を選択します。</p>
      </div>
      <button class="button secondary" @click="reset">新規</button>
    </div>
    <div class="area-list">
      <button
        v-for="group in props.groups"
        :key="group.id"
        class="area-chip"
        :class="{ selected: selected === group.id }"
        @click="selectGroup(group)"
      >
        {{ group.name }}</button
      ><span v-if="!props.groups.length" class="muted">まだ設定されていません。</span>
    </div>
    <label>エリア名<input v-model="name" placeholder="関東" /></label>
    <label
      >所属する拠点<select v-model="nodeId">
        <option value="">選択しない</option>
        <option v-for="entry in props.nodes" :key="entry.node.node_id" :value="entry.node.node_id">
          {{ entry.node.display_name }}
        </option>
      </select></label
    >
    <fieldset>
      <legend>経路選択</legend>
      <label><input v-model="preference" type="radio" value="auto" />● 自動（おすすめ）</label
      ><label><input v-model="preference" type="radio" value="priority" />○ この拠点を優先</label
      ><label
        ><input v-model="preference" type="radio" value="backup" />○ この拠点を予備として使用</label
      >
    </fieldset>
    <ul v-if="selectedGroup()?.members?.length" class="members">
      <li v-for="member in selectedGroup()?.members" :key="member.node_id">
        {{
          props.nodes.find((entry) => entry.node.node_id === member.node_id)?.node.display_name ||
          member.node_id
        }}<button class="link" @click="removeMember(member)">所属から外す</button>
      </li>
    </ul>
    <div class="actions">
      <button class="button" :disabled="saving" @click="saveGroup">
        {{ saving ? '保存中…' : '保存' }}</button
      ><button v-if="selected !== null" class="button secondary" @click="removeGroup">削除</button>
    </div>
    <p v-if="error" class="notice error" role="alert">{{ error }}</p>
  </section>
</template>
<style scoped>
.area-editor {
  display: grid;
  gap: 0.8rem;
}

.area-heading,
.area-list,
.actions {
  display: flex;
  gap: 0.6rem;
  align-items: center;
  flex-wrap: wrap;
  justify-content: space-between;
}

.area-heading h3 {
  margin: 0;
}

.area-heading p {
  margin: 0.3rem 0;
}

.area-chip {
  padding: 0.45rem 0.7rem;
  border: 1px solid var(--border, #d9dee7);
  border-radius: 999px;
  background: none;
  color: inherit;
  cursor: pointer;
}

.area-chip.selected {
  background: rgb(59 130 246 / 14%);
}

.area-editor > label {
  display: grid;
  gap: 0.3rem;
  font-weight: 600;
}

.area-editor input:not([type='radio']),
.area-editor select {
  box-sizing: border-box;
  width: 100%;
  padding: 0.6rem;
}

.area-editor fieldset {
  display: grid;
  gap: 0.4rem;
  border: 1px solid var(--border, #d9dee7);
}

.area-editor fieldset label {
  display: flex;
  gap: 0.4rem;
}
</style>

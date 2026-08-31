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
  <section class="area-editor panel">
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
    <label class="field"><span>エリア名</span><input v-model="name" placeholder="関東" /></label>
    <label class="field"
      ><span>所属する拠点</span
      ><select v-model="nodeId">
        <option value="">選択しない</option>
        <option v-for="entry in props.nodes" :key="entry.node.node_id" :value="entry.node.node_id">
          {{ entry.node.display_name }}
        </option>
      </select></label
    >
    <fieldset class="route-preference">
      <legend>経路選択</legend>
      <label class="check"
        ><input v-model="preference" type="radio" value="auto" /><span>自動（おすすめ）</span></label
      ><label class="check"
        ><input v-model="preference" type="radio" value="priority" /><span
          >この拠点を優先</span
        ></label
      ><label class="check"
        ><input v-model="preference" type="radio" value="backup" /><span
          >この拠点を予備として使用</span
        ></label
      >
    </fieldset>
    <ul v-if="selectedGroup()?.members?.length" class="members">
      <li v-for="member in selectedGroup()?.members" :key="member.node_id">
        {{
          props.nodes.find((entry) => entry.node.node_id === member.node_id)?.node.display_name ||
          member.node_id
        }}<button class="link" type="button" @click="removeMember(member)">所属から外す</button>
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
  gap: 12px;
}

.area-heading {
  display: flex;
  gap: 12px;
  align-items: center;
  flex-wrap: wrap;
  justify-content: space-between;
}

.area-heading h3 {
  margin: 0;
}

.area-heading p {
  margin: 4px 0 0;
}

/* Chips are a horizontal list, not a spread-apart row: without an explicit
   flex-start they inherit the heading's space-between and drift to the edges. */
.area-list {
  display: flex;
  gap: 8px;
  align-items: center;
  flex-wrap: wrap;
  justify-content: flex-start;
}

.area-chip {
  min-height: 36px;
  padding: 6px 14px;
  border: 1px solid var(--border);
  border-radius: 999px;
  background: var(--surface);
  color: var(--text);
  cursor: pointer;
}

.area-chip.selected {
  background: var(--soft);
  border-color: var(--accent);
  font-weight: 700;
}

/* .field carries a vertical margin for stacked forms; this panel already
   spaces its children with grid gap. */
.area-editor .field {
  margin: 0;
}

.route-preference {
  margin: 0;
  padding: 8px 14px 12px;
  border: 1px solid var(--border);
  border-radius: 8px;
}

.route-preference legend {
  padding: 0 6px;
  font-weight: 700;
}

.members {
  margin: 0;
  padding-left: 20px;
  display: grid;
  gap: 6px;
}

.members li {
  display: flex;
  gap: 10px;
  align-items: center;
  flex-wrap: wrap;
}
</style>

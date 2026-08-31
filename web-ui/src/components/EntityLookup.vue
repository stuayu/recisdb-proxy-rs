<script setup lang="ts">
import { computed, ref } from 'vue'

export type LookupEntity = { id: number | string; name: string; description?: string; secondary?: string }
const props = withDefaults(defineProps<{ modelValue: number | string | null; entities: LookupEntity[]; label?: string; disabled?: boolean; loading?: boolean; placeholder?: string }>(), { label: '選択', disabled: false, loading: false, placeholder: '名前で検索' })
const emit = defineEmits<{ 'update:modelValue': [number | string | null] }>()
const query = ref('')
const open = ref(false)
const active = ref(-1)
const selected = computed(() => props.entities.find((entity) => String(entity.id) === String(props.modelValue)))
const filtered = computed(() => props.entities.filter((entity) => `${entity.name} ${entity.description ?? ''} ${entity.secondary ?? ''}`.toLowerCase().includes(query.value.trim().toLowerCase())))
function choose(entity: LookupEntity) { emit('update:modelValue', entity.id); query.value = ''; open.value = false }
function onKeydown(event: KeyboardEvent) { if (!open.value && (event.key === 'ArrowDown' || event.key === 'Enter')) { open.value = true; return } if (event.key === 'Escape') { open.value = false; return } if (event.key === 'ArrowDown') { event.preventDefault(); active.value = Math.min(active.value + 1, filtered.value.length - 1) } else if (event.key === 'ArrowUp') { event.preventDefault(); active.value = Math.max(active.value - 1, 0) } else if (event.key === 'Enter' && filtered.value[active.value]) { event.preventDefault(); choose(filtered.value[active.value]) } }
</script>

<template>
  <div class="entity-lookup">
    <span class="lookup-label">{{ label }}</span>
    <button v-if="selected && !open" type="button" class="lookup-selected" :disabled="disabled" @click="open = true">
      <span><slot name="selected" :entity="selected"><strong v-text="selected.name" /><small v-if="selected.description" v-text="selected.description" /><small v-if="selected.secondary" v-text="selected.secondary" /></slot></span><span aria-hidden="true">変更</span>
    </button>
    <input v-else v-model="query" type="search" :placeholder="placeholder" :disabled="disabled" :aria-label="label" @focus="open = true" @keydown="onKeydown" />
    <button v-if="modelValue !== null && !disabled" type="button" class="lookup-clear" aria-label="選択を解除" @click="emit('update:modelValue', null)">×</button>
    <div v-if="open" class="lookup-options" role="listbox" :aria-label="label">
      <p v-if="loading" class="muted">読み込み中…</p><p v-else-if="!filtered.length" class="muted">候補がありません</p>
      <button v-for="(entity, index) in filtered" :key="String(entity.id)" type="button" role="option" :aria-selected="String(entity.id) === String(modelValue)" :class="{ active: index === active }" @mousedown.prevent="choose(entity)"><slot name="option" :entity="entity"><strong v-text="entity.name" /><small v-if="entity.description" v-text="entity.description" /><small v-if="entity.secondary" v-text="entity.secondary" /></slot></button>
    </div>
  </div>
</template>

<style scoped>
.entity-lookup { position: relative; display: grid; gap: 6px; }
.lookup-label { font-weight: 600; }
.lookup-selected, .lookup-options button { display: flex; justify-content: space-between; gap: 12px; width: 100%; min-height: 42px; padding: 9px 12px; text-align: left; border: 1px solid var(--border, #ccc); border-radius: 6px; background: var(--panel, #fff); cursor: pointer; }
.lookup-selected span:first-child, .lookup-options button { display: grid; gap: 2px; }
.lookup-selected small, .lookup-options small { color: var(--muted, #666); }
.lookup-clear { position: absolute; right: 5px; bottom: 5px; min-width: 32px; min-height: 32px; border: 0; background: transparent; cursor: pointer; }
.lookup-options { position: absolute; z-index: 5; top: 100%; left: 0; right: 0; max-height: 280px; overflow: auto; padding: 4px; border: 1px solid var(--border, #ccc); border-radius: 6px; background: var(--panel, #fff); box-shadow: 0 8px 20px rgb(0 0 0 / 15%); }
.lookup-options button { border: 0; border-radius: 4px; }
.lookup-options button:hover, .lookup-options button.active { background: var(--surface-hover, #eef3f8); }
</style>

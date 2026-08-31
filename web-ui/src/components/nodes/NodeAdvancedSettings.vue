<script setup lang="ts">
defineProps<{
  nodeId?: string
  endpoint?: { kind: string; address: string }
  recordAllowed: boolean
  credential: string
  weight: number
}>()
const emit = defineEmits<{
  'update:recordAllowed': [boolean]
  'update:credential': [string]
  'update:weight': [number]
}>()
</script>
<template>
  <details class="advanced">
    <summary>詳細設定（Expert Mode）</summary>
    <p class="muted">通常は変更不要。接続先や認証情報を手動管理する場合だけ使用。</p>
    <label>Node ID<input :value="nodeId" disabled /></label>
    <label>EndpointKind<input :value="endpoint?.kind" disabled /></label>
    <label>Endpoint URL<input :value="endpoint?.address" disabled /></label>
    <label class="check"
      ><input
        :checked="recordAllowed"
        type="checkbox"
        @change="emit('update:recordAllowed', ($event.target as HTMLInputElement).checked)"
      />録画経路として使用可</label
    >
    <label
      >共有credential<input
        :value="credential"
        type="password"
        autocomplete="off"
        @input="emit('update:credential', ($event.target as HTMLInputElement).value)"
    /></label>
    <label
      >weight<input
        :value="weight"
        type="number"
        min="1"
        max="10000"
        @input="emit('update:weight', Number(($event.target as HTMLInputElement).value))"
    /></label>
  </details>
</template>
<style scoped>
.advanced {
  display: grid;
  gap: 0.6rem;
}

.advanced label {
  display: grid;
  gap: 0.3rem;
  font-weight: 600;
}

.advanced input {
  padding: 0.5rem;
}

.check {
  display: flex !important;
  align-items: center;
}
</style>

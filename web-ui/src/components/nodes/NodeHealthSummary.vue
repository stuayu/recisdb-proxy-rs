<script setup lang="ts">
import type { ProbeResponse } from './types'
import { healthLabel } from './health'
defineProps<{ probe?: ProbeResponse }>()
function pathFor(probe: ProbeResponse | undefined, kind: string | null) {
  return probe?.paths.find((path) => path.id === kind)
}
</script>
<template>
  <div v-if="probe" class="health-summary" aria-live="polite">
    <span
      >応答速度
      <b :class="healthLabel(pathFor(probe, probe.selected.view))[0]"
        >● {{ healthLabel(pathFor(probe, probe.selected.view))[1] }}</b
      ></span
    >
    <span
      >通信速度
      <b :class="probe.selected.view ? 'good' : 'bad'"
        >● {{ probe.selected.view ? '十分' : '利用不可' }}</b
      ></span
    >
    <span
      >安定性
      <b :class="healthLabel(pathFor(probe, probe.selected.view))[0]"
        >● {{ healthLabel(pathFor(probe, probe.selected.view))[1] }}</b
      ></span
    >
    <span
      >ライブ視聴
      <b :class="probe.selected.view ? 'good' : 'bad'"
        >● {{ probe.selected.view ? '快適' : '利用不可' }}</b
      ></span
    >
    <span
      >録画
      <b :class="probe.selected.record ? 'good' : 'bad'"
        >● {{ probe.selected.record ? '推奨' : '利用不可' }}</b
      ></span
    >
  </div>
  <p v-else class="muted">通信テストで状態を確認できます。</p>
</template>
<style scoped>
.health-summary {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(130px, 1fr));
  gap: 8px;
}

.health-summary span {
  padding: 10px;
  background: var(--soft);
  border-radius: 8px;
}
</style>

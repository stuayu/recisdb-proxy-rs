<script setup lang="ts">
import type { ProbeResponse, ProbePath } from './types'
defineProps<{ probe?: ProbeResponse }>()
function level(path: ProbePath | undefined) {
  if (!path || path.health.state === 'unreachable') return ['bad', '利用不可']
  if (path.health.state === 'degraded') return ['warn', '不安定']
  if (path.health.rtt_p95_ms <= 30 && path.health.stall_rate < 0.02) return ['good', 'とても良好']
  return ['good', '良好']
}
function pathFor(probe: ProbeResponse | undefined, kind: string | null) {
  return probe?.paths.find((path) => path.id === kind)
}
</script>
<template>
  <div v-if="probe" class="health-summary" aria-live="polite">
    <span
      >応答速度
      <b :class="level(pathFor(probe, probe.selected.view))[0]"
        >● {{ level(pathFor(probe, probe.selected.view))[1] }}</b
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
      <b :class="level(pathFor(probe, probe.selected.view))[0]"
        >● {{ level(pathFor(probe, probe.selected.view))[1] }}</b
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
  gap: 0.5rem;
}

.health-summary span {
  padding: 0.6rem;
  background: rgb(127 127 127 / 8%);
  border-radius: 8px;
}

.good {
  color: #15803d;
}

.warn {
  color: #a16207;
}

.bad {
  color: #b91c1c;
}
</style>

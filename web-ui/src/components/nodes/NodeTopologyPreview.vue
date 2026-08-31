<script setup lang="ts">
import type { Topology } from './types'
const props = defineProps<{ topology: Topology; focusNodeId?: string }>()
const emit = defineEmits<{ probe: [] }>()
function label(path: Topology['paths'][number]) {
  if (!path.measured) return '⚪ 未測定・通信テストが必要'
  if (path.status === 'degraded') return '🟡 不安定・通信テストを再実行'
  if (path.status === 'unavailable') return '🔴 利用不可・原因を確認'
  if (path.kind === 'cloudflare_public' || !path.record_allowed)
    return '🟡 視聴には使用できます / 🚫 録画には使用しません'
  return path.role === 'primary' ? '🟢 おすすめ・録画利用可' : '🟡 予備・録画利用可'
}
function metrics(path: Topology['paths'][number]) {
  if (!path.measured || !path.health) return ''
  const rtt = typeof path.health.rtt_p95_ms === 'number' && Number.isFinite(path.health.rtt_p95_ms)
    ? ` / ${path.health.rtt_p95_ms.toFixed(1)} ms`
    : ''
  const bandwidth = path.health.throughput_down_p10_bps
    ? ` / ${(path.health.throughput_down_p10_bps / 1_000_000).toFixed(1)} Mbps`
    : ''
  return `${rtt}${bandwidth}`
}
function x(index: number) {
  return 280 + (index % 2) * 220
}
function pathFor(nodeId: string) {
  return props.topology.paths.filter((path) => path.to === nodeId)
}
</script>
<template>
  <div class="topology-wrap panel">
    <svg
      class="topology-svg desktop-svg"
      viewBox="0 0 720 260"
      role="img"
      aria-labelledby="topology-title topology-desc"
    >
      <title id="topology-title">分散ノード構成図</title>
      <desc id="topology-desc">{{ topology.local.display_name }}から接続先拠点への通信経路</desc>
      <g
        v-for="(node, index) in topology.nodes.filter(
          (item) => !focusNodeId || item.node_id === focusNodeId,
        )"
        :key="node.node_id"
      >
        <line
          v-for="(path, pathIndex) in pathFor(node.node_id)"
          :key="`${path.to}-${pathIndex}`"
          x1="180"
          y1="80"
          :x2="x(pathIndex)"
          y2="180"
          :class="path.status === 'online' ? 'healthy' : path.status === 'unmeasured' ? 'unmeasured' : 'unavailable'"
        />
        <text
          v-for="(path, pathIndex) in pathFor(node.node_id)"
          :key="`${path.to}-${pathIndex}-label`"
          :x="x(pathIndex) - 55"
          :y="130 + pathIndex * 18"
          class="path-label"
        >
          {{ path.kind }}: {{ label(path) }}{{ metrics(path) }}
        </text>
        <rect :x="x(index) - 80" y="170" width="160" height="55" rx="8" class="node-box" />
        <text :x="x(index)" y="195" text-anchor="middle" class="node-name">
          {{ node.display_name }}
        </text>
        <text :x="x(index)" y="213" text-anchor="middle" class="node-sub">受信拠点</text>
      </g>
      <rect x="100" y="35" width="160" height="55" rx="8" class="node-box local-box" />
      <text x="180" y="60" text-anchor="middle" class="node-name">
        {{ topology.local.display_name }}
      </text>
      <text x="180" y="78" text-anchor="middle" class="node-sub">視聴・録画</text>
    </svg>
    <svg
      class="topology-svg mobile-svg"
      viewBox="0 0 720 440"
      role="img"
      aria-labelledby="topology-mobile-title topology-mobile-desc"
    >
      <title id="topology-mobile-title">分散ノード構成図（縦表示）</title>
      <desc id="topology-mobile-desc">{{ topology.local.display_name }}から各拠点への通信経路</desc>
      <rect x="280" y="10" width="160" height="55" rx="8" class="node-box local-box" />
      <text x="360" y="35" text-anchor="middle" class="node-name">
        {{ topology.local.display_name }}
      </text>
      <text x="360" y="53" text-anchor="middle" class="node-sub">視聴・録画</text>
      <g
        v-for="(node, index) in topology.nodes.filter(
          (item) => !focusNodeId || item.node_id === focusNodeId,
        )"
        :key="`mobile-${node.node_id}`"
      >
        <line x1="360" y1="65" x2="360" :y2="125 + index * 130" class="healthy" />
        <text
          v-for="(path, pathIndex) in pathFor(node.node_id)"
          :key="`${path.to}-mobile-${pathIndex}`"
          x="375"
          :y="105 + index * 130 + pathIndex * 16"
          class="path-label"
        >
          {{ path.kind }}: {{ label(path) }}{{ metrics(path) }}
        </text>
        <rect x="280" :y="125 + index * 130" width="160" height="55" rx="8" class="node-box" />
        <text x="360" :y="150 + index * 130" text-anchor="middle" class="node-name">
          {{ node.display_name }}
        </text>
        <text x="360" :y="168 + index * 130" text-anchor="middle" class="node-sub">受信拠点</text>
      </g>
    </svg>
    <div class="accessible-paths">
      <div
        v-for="path in topology.paths.filter((item) => !focusNodeId || item.to === focusNodeId)"
        :key="`${path.to}-${path.kind}`"
      >
        <strong>{{ path.kind }}</strong> {{ label(path) }}{{ metrics(path) }}
        <button v-if="!path.measured" class="link" type="button" @click="emit('probe')">通信テスト</button>
      </div>
      <p v-if="!topology.paths.length" class="muted">利用可能な経路なし</p>
    </div>
  </div>
</template>
<style scoped>
.topology-wrap {
  display: grid;
  gap: 12px;
}

.topology-svg {
  width: 100%;
  min-height: 230px;
}

.node-box {
  fill: var(--surface);
  stroke: var(--border);
  stroke-width: 2;
}

.local-box {
  stroke: var(--accent);
}

.node-name {
  fill: currentcolor;
  font-weight: 700;
  font-size: 15px;
}

.node-sub,
.path-label {
  fill: var(--muted);
  font-size: 11px;
}

.healthy {
  stroke: var(--success);
  stroke-width: 3;
}

.unavailable {
  stroke: var(--danger);
  stroke-width: 2;
  stroke-dasharray: 5 4;
}

.unmeasured {
  stroke: var(--muted);
  stroke-width: 2;
  stroke-dasharray: 3 4;
}

.accessible-paths {
  display: grid;
  gap: 6px;
  font-size: 0.9rem;
}

.accessible-paths p {
  margin: 0;
}

.mobile-svg {
  display: none;
}

@media (max-width: 700px) {
  .desktop-svg {
    display: none;
  }

  .mobile-svg {
    display: block;
    min-height: 400px;
  }
}
</style>

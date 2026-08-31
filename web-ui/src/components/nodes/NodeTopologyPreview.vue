<script setup lang="ts">
defineProps<{
  local: { display_name: string }
  node: { display_name: string; site_name: string | null }
  path?: string
}>()
</script>
<template>
  <div
    class="topology"
    role="img"
    :aria-label="`${local.display_name}から${node.display_name}への構成図`"
  >
    <div class="topology-box">
      <strong>{{ local.display_name }}</strong
      ><small>視聴・録画</small>
    </div>
    <div class="topology-link">
      <span>{{ path || '利用可能な経路' }}</span
      ><b>↓</b>
    </div>
    <div class="topology-box">
      <strong>{{ node.display_name }}</strong
      ><small>{{ node.site_name || '受信拠点' }}</small>
    </div>
  </div>
</template>
<style scoped>
.topology {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 1rem;
  padding: 1rem;
  border: 1px solid var(--border, #d9dee7);
  border-radius: 10px;
}

.topology-box {
  min-width: 9rem;
  padding: 0.8rem;
  text-align: center;
  border: 1px solid var(--border, #d9dee7);
  border-radius: 8px;
  background: rgb(127 127 127 / 8%);
}

.topology-box small {
  display: block;
  color: var(--text-muted, #6b7280);
}

.topology-link {
  display: grid;
  text-align: center;
  color: var(--text-muted, #6b7280);
}

@media (max-width: 700px) {
  .topology {
    flex-direction: column;
  }

  .topology-link b {
    transform: rotate(0);
  }
}
</style>

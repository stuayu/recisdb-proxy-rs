<script setup lang="ts">
defineProps<{ open: boolean }>()
const emit = defineEmits<{ close: [] }>()
function keydown(event: KeyboardEvent) {
  if (event.key === 'Escape') emit('close')
}
</script>
<template>
  <aside
    v-if="open"
    class="drawer"
    role="dialog"
    aria-modal="true"
    aria-labelledby="nodes-help-title"
    tabindex="-1"
    @keydown="keydown"
  >
    <button class="close" aria-label="設定ガイドを閉じる" @click="emit('close')">×</button>
    <h2 id="nodes-help-title">設定ガイド</h2>
    <p>別のPCを追加すると、相手拠点のチューナーを視聴・録画に利用できます。</p>
    <h3>迷った場合</h3>
    <p>通信方法は「自動」のまま。相手PCで発行した接続情報を貼り付けるだけ。</p>
    <h3>録画について</h3>
    <p>
      Cloudflare
      Publicなど録画不可の経路は、視聴だけに使います。録画は別の利用可能な経路へ自動的に分けます。
    </p>
  </aside>
</template>
<style scoped>
.drawer {
  position: fixed;
  z-index: 10;
  right: 0;
  top: 0;
  width: min(24rem, 100vw);
  height: 100vh;
  box-sizing: border-box;
  padding: 1.5rem;
  background: var(--surface, #fff);
  box-shadow: -4px 0 18px #0003;
}

.close {
  float: right;
  font-size: 1.5rem;
  border: 0;
  background: none;
  cursor: pointer;
}
</style>

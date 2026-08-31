<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
const props = defineProps<{ open: boolean }>()
const emit = defineEmits<{ close: [] }>()
const drawer = ref<HTMLElement | null>(null)
function keydown(event: KeyboardEvent) {
  if (event.key === 'Escape') emit('close')
  if (event.key !== 'Tab' || !drawer.value) return
  const focusable = [...drawer.value.querySelectorAll<HTMLElement>('button, [tabindex]:not([tabindex="-1"])')]
  if (!focusable.length) return
  const first = focusable[0]
  const last = focusable[focusable.length - 1]
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault()
    last.focus()
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault()
    first.focus()
  }
}
onMounted(() => {
  document.addEventListener('keydown', keydown)
})
onBeforeUnmount(() => document.removeEventListener('keydown', keydown))
watch(() => props.open, (open) => {
  if (open) void nextTick(() => drawer.value?.focus())
})
</script>
<template>
  <aside
    v-if="open"
    ref="drawer"
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

  /* Above the nav (z:10) but below dialogs (z:200), matching styles.css. */
  z-index: 150;
  right: 0;
  top: 0;
  width: min(24rem, 100vw);
  height: 100dvh;
  overflow: auto;
  padding: 24px;
  background: var(--surface);
  color: var(--text);
  border-left: 1px solid var(--border);
  box-shadow: -4px 0 18px #0003;
}

.close {
  float: right;
  min-width: 44px;
  min-height: 44px;
  border: 0;
  background: none;
  color: var(--text);
  font-size: 1.5rem;
  cursor: pointer;
}
</style>

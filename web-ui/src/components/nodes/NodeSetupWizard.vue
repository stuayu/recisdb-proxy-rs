<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref } from 'vue'
import { api } from '../../api'
import type { IssuedPairing, ProbeResponse } from './types'
import { nodeError } from './errors'
import { parsePairingConnection } from './pairing'
const emit = defineEmits<{ close: []; complete: [] }>()
const step = ref(1)
const purpose = ref('both')
const connection = ref('')
const issuing = ref(false)
const redeeming = ref(false)
const issued = ref<IssuedPairing | null>(null)
const error = ref('')
const diagnostic = ref<ProbeResponse | null>(null)
const rolledBack = ref(false)
const rollbackNodeId = ref<string | null>(null)
const dialog = ref<HTMLElement | null>(null)
function keydown(event: KeyboardEvent) {
  if (event.key === 'Escape') {
    emit('close')
    return
  }
  if (event.key !== 'Tab' || !dialog.value) return
  const focusable = [...dialog.value.querySelectorAll<HTMLElement>('button, input, select, [tabindex]:not([tabindex="-1"])')]
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
  void nextTick(() => dialog.value?.focus())
})
onBeforeUnmount(() => document.removeEventListener('keydown', keydown))
async function issue() {
  issuing.value = true
  error.value = ''
  try {
    issued.value = await api<IssuedPairing>('/nodes/pairing', {
      method: 'POST',
      body: JSON.stringify({ label: null }),
    })
    step.value = 2
  } catch (cause) {
    error.value = nodeError(cause)
  } finally {
    issuing.value = false
  }
}
async function redeem() {
  let pair
  try {
    pair = parsePairingConnection(connection.value)
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
    return
  }
  if (!pair.base_url || !pair.code) {
    error.value = '接続情報を確認してください。'
    return
  }
  redeeming.value = true
  error.value = ''
  try {
    const result = await api<{ node: { node_id: string } }>('/nodes/pairing/redeem', {
      method: 'POST',
      body: JSON.stringify({ base_url: pair.base_url, code: pair.code, endpoints: [] }),
    })
    rollbackNodeId.value = result.node.node_id
    diagnostic.value = await api<ProbeResponse>(
      `/nodes/${encodeURIComponent(result.node.node_id)}/probe`,
      {
        method: 'POST',
        body: JSON.stringify({ bitrate_bps: 20_000_000, download_bytes: 1_048_576 }),
      },
    )
    if (!diagnostic.value.selected.view) {
      await rollback()
      error.value = '通信経路を確認できなかったため、仮登録を取り消しました。'
    }
    step.value = 4
  } catch (cause) {
    await rollback()
    error.value = nodeError(cause)
  } finally {
    redeeming.value = false
  }
}
async function rollback() {
  if (!rollbackNodeId.value) return
  try {
    await api(`/nodes/${encodeURIComponent(rollbackNodeId.value)}`, { method: 'DELETE' })
    rolledBack.value = true
  } catch (cause) {
    error.value = `診断失敗。自動取消にも失敗しました: ${nodeError(cause)}`
  } finally {
    rollbackNodeId.value = null
  }
}
function retry() {
  step.value = 1
  issued.value = null
  connection.value = ''
  diagnostic.value = null
  rolledBack.value = false
  error.value = ''
}
async function copyPairing() {
  if (!issued.value) return
  const endpoint = issued.value.node_listen_addr || ''
  const text = `recisdb://pair?endpoint=${encodeURIComponent(endpoint)}&code=${encodeURIComponent(issued.value.code)}`
  await navigator.clipboard?.writeText(text)
}
</script>
<template>
  <div ref="dialog" class="overlay" role="dialog" aria-modal="true" aria-labelledby="wizard-title" tabindex="-1">
    <section class="wizard">
      <button class="close" aria-label="ウィザードを閉じる" @click="emit('close')">×</button>
      <p class="step">STEP {{ step }} / 4</p>
      <h2 id="wizard-title">別のPC・拠点を追加</h2>
      <div v-if="step === 1">
        <h3>別のPCと何をしたいですか？</h3>
        <label
          v-for="item in [
            { v: 'remote', t: '別のPCのチューナーを使いたい' },
            { v: 'share', t: 'このPCのチューナーを共有したい' },
            { v: 'both', t: '両方（おすすめ）' },
          ]"
          :key="item.v"
          class="choice"
          ><input v-model="purpose" type="radio" :value="item.v" />{{ item.t }}</label
        ><button class="button" @click="issue">{{ issuing ? '準備中…' : '次へ' }}</button>
      </div>
      <div v-else-if="step === 2">
        <h3>相手PCの接続情報</h3>
        <p>相手PCで発行した接続情報を貼り付け。</p>
        <div v-if="issued" class="pair-code">
          <code>{{ issued.code }}</code
          ><button class="button secondary" @click="copyPairing">接続情報をコピー</button
          ><small>コードは一度だけ表示。</small>
        </div>
        <label
          >接続情報<input
            v-model="connection"
            placeholder="recisdb://pair?..."
            autocomplete="off" /></label
        ><button class="button" @click="step = 3">次へ</button
        ><button class="button secondary" @click="step = 1">戻る</button>
      </div>
      <div v-else-if="step === 3">
        <h3>通信方法</h3>
        <label class="choice"><input checked type="radio" />自動（おすすめ）</label>
        <p class="muted">
          利用可能な通信方法を自動確認し、推奨経路を選択します。経路選択は現在、静的な優先順が基本です。
        </p>
        <details>
          <summary>通信方法を手動指定</summary>
          <p>LAN → Tailscale → Cloudflare Private → Static → Direct HTTPS → Cloudflare Public</p>
        </details>
        <button class="button" :disabled="redeeming" @click="redeem">
          {{ redeeming ? '診断中…' : '接続確認へ' }}</button
        ><button class="button secondary" @click="step = 2">戻る</button>
      </div>
      <div v-else>
        <h3>設定内容を確認</h3>
        <p>
          診断が完了。用途:
          {{
            purpose === 'both'
              ? 'ライブ視聴・録画'
              : purpose === 'remote'
                ? 'ライブ視聴'
                : 'チューナー共有'
          }}
        </p>
        <div class="diagnostic" aria-live="polite">
          <p>診断</p>
          <p class="good">✓ 相手PCへ接続可能 / ✓ 認証成功</p>
          <p :class="diagnostic?.selected.view ? 'good' : 'bad'">
            {{
              diagnostic?.selected.view
                ? '✓ 通信速度十分 / ✓ ライブ視聴 快適'
                : '⚠ ライブ視聴に利用できる経路なし'
            }}
          </p>
          <p :class="diagnostic?.selected.record ? 'good' : 'warn'">
            {{
              diagnostic?.selected.record
                ? '✓ 録画利用可能 / 録画 推奨'
                : '⚠ 録画利用可能な経路なし'
            }}
          </p>
        </div>
        <p v-if="!diagnostic?.selected.view" class="notice error" role="alert">
          問題があります。再試行するか、詳細設定の手動接続を確認してください。
        </p>
        <p v-if="rolledBack" class="notice warning" role="status">
          登録前診断に失敗したため、仮登録を自動で取り消しました。ユーザー操作は不要です。
        </p>
        <button v-if="!rolledBack && diagnostic?.selected.view" class="button" @click="emit('complete')">
          この設定で開始
        </button>
        <button v-else class="button secondary" @click="retry">再試行</button>
      </div>
      <p v-if="error" class="notice error" role="alert">
        接続設定を完了できません。詳細: {{ error }}
      </p>
    </section>
  </div>
</template>
<style scoped>
.overlay {
  position: fixed;
  inset: 0;
  z-index: 20;
  display: grid;
  place-items: center;
  padding: 1rem;
  background: #0008;
}

.wizard {
  position: relative;
  width: min(34rem, 100%);
  display: grid;
  gap: 1rem;
  padding: 1.5rem;
  background: var(--surface, #fff);
  border-radius: 14px;
  box-sizing: border-box;
}

.close {
  position: absolute;
  right: 1rem;
  top: 1rem;
  border: 0;
  background: none;
  font-size: 1.5rem;
  cursor: pointer;
}

.step {
  margin: 0;
  color: var(--text-muted, #6b7280);
}

.choice {
  display: flex;
  gap: 0.6rem;
  align-items: center;
  padding: 0.8rem;
  border: 1px solid var(--border, #d9dee7);
  border-radius: 8px;
}

.pair-code {
  display: grid;
  gap: 0.5rem;
  padding: 0.8rem;
  background: rgb(127 127 127 / 8%);
}

.pair-code code {
  font-size: 1.2rem;
  overflow-wrap: anywhere;
}

.wizard label:not(.choice) {
  display: grid;
  gap: 0.3rem;
}

.wizard input:not([type='radio']) {
  padding: 0.6rem;
}

.good {
  color: #15803d;
}

.notice.error {
  color: #b91c1c;
}
</style>

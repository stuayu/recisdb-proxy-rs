<script setup lang="ts">
import QRCode from 'qrcode'
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { api } from '../../api'
import type { IssuedPairing, ProbeResponse } from './types'
import { nodeError } from './errors'
import { isPairingExpired, pairingConnectionText, parsePairingConnection } from './pairing'
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
const reciprocalWarning = ref('')
const rollbackNodeId = ref<string | null>(null)
const dialog = ref<HTMLElement | null>(null)
const qrCanvas = ref<HTMLCanvasElement | null>(null)
const now = ref(Date.now())
const expired = computed(() => !!issued.value && isPairingExpired(issued.value.expires_at_unix_ms, now.value))
const pairingText = computed(() =>
  issued.value ? pairingConnectionText(issued.value.node_listen_addr || '', issued.value.code) : '',
)
let clock: ReturnType<typeof setInterval> | undefined
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
  clock = setInterval(() => { now.value = Date.now() }, 1000)
})
onBeforeUnmount(() => {
  document.removeEventListener('keydown', keydown)
  if (clock) clearInterval(clock)
})
watch([issued, step], () => {
  if (!qrCanvas.value || !pairingText.value || expired.value) return
  void QRCode.toCanvas(qrCanvas.value, pairingText.value, {
    errorCorrectionLevel: 'M',
    margin: 2,
    width: 240,
  })
})
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
    const result = await api<{ reciprocal_removed?: boolean; warning?: string }>(
      `/nodes/${encodeURIComponent(rollbackNodeId.value)}`,
      { method: 'DELETE' },
    )
    rolledBack.value = true
    if (!result.reciprocal_removed) reciprocalWarning.value = result.warning || '相手側の設定が残っている可能性があります。相手PCの分散ノード一覧から、このPCの接続設定を削除してください。'
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
  reciprocalWarning.value = ''
}
async function copyPairing() {
  if (!issued.value) return
  await navigator.clipboard?.writeText(pairingText.value)
}
</script>
<template>
  <div ref="dialog" class="dialog-backdrop" role="dialog" aria-modal="true" aria-labelledby="wizard-title" tabindex="-1">
    <section class="dialog wizard">
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
          class="choice check"
          ><input v-model="purpose" type="radio" :value="item.v" /><span>{{ item.t }}</span></label
        >
        <div class="actions">
          <button class="button" @click="issue">{{ issuing ? '準備中…' : '次へ' }}</button>
        </div>
      </div>
      <div v-else-if="step === 2">
        <h3>相手PCの接続情報</h3>
        <p>相手PCで発行した接続情報を貼り付け。</p>
        <div v-if="issued" class="pair-code">
          <code>{{ issued.code }}</code>
          <p v-if="expired" class="notice error" role="alert">
            接続情報の有効期限が切れています。再発行してください。
          </p>
          <div v-else class="qr-box">
            <canvas ref="qrCanvas" role="img" aria-label="ペアリング接続情報のQRコード" />
            <span>QRを読み取って接続できます。</span>
          </div>
          <code class="pairing-text">{{ pairingText }}</code>
          <div class="actions">
            <button class="button secondary" :disabled="expired" @click="copyPairing">
              接続情報をコピー
            </button>
          </div>
          <small>コードは一度だけ表示。文字列でも接続できます。</small>
          <div v-if="expired" class="actions">
            <button class="button" @click="issue">接続情報を再発行</button>
          </div>
        </div>
        <label class="field"
          ><span>接続情報</span
          ><input v-model="connection" placeholder="recisdb://pair?..." autocomplete="off"
        /></label>
        <div class="actions">
          <button class="button" @click="step = 3">次へ</button
          ><button class="button secondary" @click="step = 1">戻る</button>
        </div>
      </div>
      <div v-else-if="step === 3">
        <h3>通信方法</h3>
        <label class="choice check"
          ><input checked type="radio" /><span>自動（おすすめ）</span></label
        >
        <p class="muted">
          利用可能な通信方法を自動確認し、推奨経路を選択します。経路選択は現在、静的な優先順が基本です。
        </p>
        <details>
          <summary>通信方法を手動指定</summary>
          <p>LAN → Tailscale → Cloudflare Private → Static → Direct HTTPS → Cloudflare Public</p>
        </details>
        <div class="actions">
          <button class="button" :disabled="redeeming" @click="redeem">
            {{ redeeming ? '診断中…' : '接続確認へ' }}</button
          ><button class="button secondary" @click="step = 2">戻る</button>
        </div>
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
        <p v-if="reciprocalWarning" class="notice warning" role="alert">{{ reciprocalWarning }}</p>
        <div class="actions">
          <button
            v-if="!rolledBack && diagnostic?.selected.view"
            class="button"
            @click="emit('complete')"
          >
            この設定で開始
          </button>
          <button v-else class="button secondary" @click="retry">再試行</button>
        </div>
      </div>
      <p v-if="error" class="notice error" role="alert">
        接続設定を完了できません。詳細: {{ error }}
      </p>
    </section>
  </div>
</template>
<style scoped>
.wizard {
  position: relative;
  width: min(34rem, 100%);
  max-height: 90dvh;
  overflow: auto;
  display: grid;
  gap: 16px;
}

.wizard > div {
  display: grid;
  gap: 12px;
}

.close {
  position: absolute;
  right: 12px;
  top: 12px;
  min-width: 44px;
  min-height: 44px;
  border: 0;
  background: none;
  color: var(--text);
  font-size: 1.5rem;
  cursor: pointer;
}

.step {
  margin: 0;
  color: var(--muted);
}

.choice {
  padding: 8px 14px;
  border: 1px solid var(--border);
  border-radius: 8px;
}

.pair-code {
  display: grid;
  gap: 8px;
  padding: 12px;
  background: var(--soft);
  border-radius: 8px;
}

.pair-code code {
  font-size: 1.2rem;
  overflow-wrap: anywhere;
}

.qr-box {
  display: grid;
  justify-items: center;
  gap: 6px;
}

.qr-box canvas {
  width: min(240px, 100%);
  height: auto;
  image-rendering: pixelated;
}

.pairing-text {
  font-size: 0.8rem !important;
  overflow-wrap: anywhere;
}

/* .field's stacked-form margin would fight the dialog's grid gap. */
.wizard .field {
  margin: 0;
}

.diagnostic {
  display: grid;
  gap: 4px;
  padding: 12px;
  background: var(--soft);
  border-radius: 8px;
}

.diagnostic p {
  margin: 0;
}
</style>

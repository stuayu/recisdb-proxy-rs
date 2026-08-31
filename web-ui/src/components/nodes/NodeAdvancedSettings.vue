<script setup lang="ts">
import { ref, watch } from 'vue'
import { api } from '../../api'
import type { NodeEntry, EndpointKind } from './types'
const props = defineProps<{ entry?: NodeEntry; open?: boolean }>()
const emit = defineEmits<{ saved: []; error: [string] }>()
const nodeId = ref('')
const displayName = ref('')
const siteName = ref('')
const kind = ref<EndpointKind>('tailscale')
const address = ref('')
const recordAllowed = ref(true)
const autoConnect = ref(true)
const credential = ref('')
const saving = ref(false)
const error = ref('')
const saved = ref(false)
function sync() {
  const endpoint = props.entry?.endpoints[0]
  nodeId.value = props.entry?.node.node_id || ''
  displayName.value = props.entry?.node.display_name || ''
  siteName.value = props.entry?.node.site_name || ''
  kind.value = endpoint?.kind || 'tailscale'
  address.value = endpoint?.address || ''
  recordAllowed.value = endpoint?.record_allowed ?? true
  autoConnect.value = props.entry?.node.auto_connect ?? true
}
watch(() => props.entry, sync, { immediate: true })
async function save() {
  if (!nodeId.value.trim() || !displayName.value.trim() || !address.value.trim()) {
    error.value = 'Node ID・表示名・接続先を入力してください。'
    return
  }
  if (!/^https?:\/\//.test(address.value.trim())) {
    error.value = '接続先はhttp://またはhttps://で始めてください。'
    return
  }
  saving.value = true
  error.value = ''
  saved.value = false
  try {
    await api('/nodes', {
      method: 'POST',
      body: JSON.stringify({
        node_id: nodeId.value.trim(),
        display_name: displayName.value.trim(),
        site_name: siteName.value.trim() || null,
        enabled: true,
        allow_transit: false,
        auto_connect: autoConnect.value,
        credential: credential.value.trim() || null,
        endpoints: [
          {
            kind: kind.value,
            address: address.value.trim(),
            enabled: true,
            record_allowed: recordAllowed.value,
            metered: false,
            user_priority: 0,
          },
        ],
      }),
    })
    saved.value = true
    credential.value = ''
    emit('saved')
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
    emit('error', error.value)
  } finally {
    saving.value = false
  }
}
</script>
<template>
  <details class="advanced" :open="open">
    <summary>詳細設定（Expert Mode）</summary>
    <p class="muted">通常は変更不要。接続先や認証情報を手動管理する場合だけ使用。</p>
    <label
      >Node ID<input
        v-model="nodeId"
        :disabled="!!entry"
        autocomplete="off"
        placeholder="tokyo" /></label
    ><label>表示名<input v-model="displayName" autocomplete="off" placeholder="東京" /></label
    ><label>受信拠点<input v-model="siteName" autocomplete="off" placeholder="東京都" /></label
    ><label
      >EndpointKind<select v-model="kind">
        <option value="lan">LAN</option>
        <option value="tailscale">Tailscale</option>
        <option value="cloudflare_private">Cloudflare Private</option>
        <option value="internet_direct">Direct HTTPS</option>
        <option value="cloudflare_public">Cloudflare Public</option>
        <option value="static">Static</option>
      </select></label
    ><label
      >Endpoint URL<input
        v-model="address"
        autocomplete="off"
        placeholder="http://100.x.y.z:20773" /></label
    ><label class="check"
      ><input v-model="recordAllowed" type="checkbox" />録画経路として使用可</label
    ><label class="check"><input v-model="autoConnect" type="checkbox" />自動接続</label
    ><label
      >共有credential<input
        v-model="credential"
        type="password"
        autocomplete="off"
        placeholder="未入力なら既存値を保持"
    /></label>
    <button class="button" :disabled="saving" @click.prevent="save">
      {{ saving ? '保存中…' : '詳細設定を保存' }}
    </button>
    <p v-if="saved" class="success" role="status">設定を保存しました。</p>
    <p v-if="error" class="notice error" role="alert">{{ error }}</p>
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

.advanced input,
.advanced select {
  box-sizing: border-box;
  width: 100%;
  padding: 0.5rem;
}

.check {
  display: flex !important;
  align-items: center;
}

.success {
  color: #15803d;
}
</style>

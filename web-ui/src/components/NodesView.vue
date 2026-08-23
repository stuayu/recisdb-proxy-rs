<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api } from '../api'

type EndpointKind =
  | 'lan'
  | 'internet_direct'
  | 'tailscale'
  | 'cloudflare_private'
  | 'cloudflare_public'
  | 'static'

type NodeEndpoint = {
  kind: EndpointKind
  address: string
  enabled: boolean
  record_allowed: boolean
  metered: boolean
  user_priority: number
}

type StoredNode = {
  node_id: string
  display_name: string
  site_name: string | null
  enabled: boolean
  allow_transit: boolean
  auto_connect: boolean
  last_seen_unix_ms: number | null
}

type NodeEntry = {
  node: StoredNode
  endpoints: NodeEndpoint[]
  paired: boolean
  /// Advertised routes this node can currently be asked for.
  routable_routes: number
  /// Everything it advertised, including quarantined/disabled routes that are
  /// kept for re-probing.
  total_routes: number
}

type RouteGroup = { id: number; name: string }

type PendingPairing = {
  label: string | null
  expires_at_unix_ms: number
  created_at_unix_ms: number
}

type NodesResponse = {
  success: boolean
  local: { node_id: string; display_name: string }
  nodes: NodeEntry[]
  route_groups: RouteGroup[]
  pending_pairings: PendingPairing[]
}

type IssuedPairing = {
  success: boolean
  code: string
  expires_at_unix_ms: number
  ttl_secs: number
  label: string | null
  node_listen_addr: string | null
}

type ProbePath = {
  id: string
  endpoint: NodeEndpoint
  health: {
    state: string
    connect_success_rate: number
    rtt_p50_ms: number
    rtt_p95_ms: number
    throughput_down_p10_bps: number
    throughput_down_ewma_bps: number
    jitter_ms: number
    stall_rate: number
    reconnect_rate: number
    confidence: number
    tailscale_path: string | null
  }
}

type ProbeResponse = {
  success: boolean
  bitrate_bps: number
  paths: ProbePath[]
  selected: { view: string | null; preview: string | null; record: string | null }
}

const data = ref<NodesResponse | null>(null)
const loading = ref(false)
const saving = ref(false)
const error = ref('')
const message = ref('')
const probing = ref<string | null>(null)
const probes = ref<Record<string, ProbeResponse>>({})

const form = ref({
  node_id: '',
  display_name: '',
  site_name: '',
  kind: 'tailscale' as EndpointKind,
  address: '',
  record_allowed: true,
  auto_connect: true,
  credential: '',
})

const groupForm = ref({ name: '関東', node_id: '', weight: 100 })
const pairIssueForm = ref({ label: '' })
const pairRedeemForm = ref({ base_url: '', code: '' })
// Shown once, in this browser tab only. The server keeps only the digest, so
// there is no endpoint that can hand it back.
const issuedCode = ref<IssuedPairing | null>(null)
const pairing = ref(false)
const endpointKinds: Array<{ value: EndpointKind; label: string }> = [
  { value: 'lan', label: 'LAN' },
  { value: 'tailscale', label: 'Tailscale' },
  { value: 'cloudflare_private', label: 'Cloudflare Private' },
  { value: 'internet_direct', label: 'Direct HTTPS' },
  { value: 'cloudflare_public', label: 'Cloudflare Public' },
  { value: 'static', label: 'Static' },
]

const nodes = computed(() => data.value?.nodes ?? [])

function formatMbps(bps: number) {
  if (!Number.isFinite(bps) || bps <= 0) return '—'
  return `${(bps / 1_000_000).toFixed(1)} Mbps`
}
function formatMs(ms: number) {
  return Number.isFinite(ms) ? `${ms.toFixed(1)} ms` : '—'
}
function pathLabel(path: ProbePath) {
  const via = path.health.tailscale_path ? ` / ${path.health.tailscale_path}` : ''
  return `${path.endpoint.kind}${via}`
}

async function load() {
  loading.value = true
  try {
    data.value = await api<NodesResponse>('/nodes')
    error.value = ''
    if (!groupForm.value.node_id && nodes.value[0]) groupForm.value.node_id = nodes.value[0].node.node_id
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    loading.value = false
  }
}

function edit(entry: NodeEntry) {
  const endpoint = entry.endpoints[0]
  form.value = {
    node_id: entry.node.node_id,
    display_name: entry.node.display_name,
    site_name: entry.node.site_name ?? '',
    kind: endpoint?.kind ?? 'tailscale',
    address: endpoint?.address ?? '',
    record_allowed: endpoint?.record_allowed ?? true,
    auto_connect: entry.node.auto_connect,
    credential: '',
  }
  window.scrollTo({ top: 0, behavior: 'smooth' })
}

function resetForm() {
  form.value = {
    node_id: '', display_name: '', site_name: '', kind: 'tailscale', address: '',
    record_allowed: true, auto_connect: true, credential: '',
  }
}

async function save() {
  if (!form.value.node_id.trim() || !form.value.display_name.trim() || !form.value.address.trim()) {
    error.value = 'ノードID・表示名・接続先を入力してください。'
    return
  }
  saving.value = true
  try {
    await api('/nodes', {
      method: 'POST',
      body: JSON.stringify({
        node_id: form.value.node_id.trim(),
        display_name: form.value.display_name.trim(),
        site_name: form.value.site_name.trim() || null,
        enabled: true,
        allow_transit: false,
        auto_connect: form.value.auto_connect,
        credential: form.value.credential.trim() || null,
        endpoints: [{
          kind: form.value.kind,
          address: form.value.address.trim(),
          enabled: true,
          record_allowed: form.value.record_allowed,
          metered: false,
          user_priority: 0,
        }],
      }),
    })
    message.value = 'ノード設定を保存しました。'
    error.value = ''
    resetForm()
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    saving.value = false
  }
}

async function probe(entry: NodeEntry) {
  const id = entry.node.node_id
  probing.value = id
  try {
    probes.value[id] = await api<ProbeResponse>(`/nodes/${encodeURIComponent(id)}/probe`, {
      method: 'POST',
      body: JSON.stringify({ bitrate_bps: 20_000_000, download_bytes: 1_048_576 }),
    })
    error.value = ''
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    probing.value = null
  }
}

function formatExpiry(unixMs: number) {
  const remainingMs = unixMs - Date.now()
  if (remainingMs <= 0) return '期限切れ'
  return `あと約 ${Math.max(1, Math.round(remainingMs / 60000))} 分`
}

async function issuePairingCode() {
  pairing.value = true
  try {
    issuedCode.value = await api<IssuedPairing>('/nodes/pairing', {
      method: 'POST',
      body: JSON.stringify({ label: pairIssueForm.value.label.trim() || null }),
    })
    error.value = ''
    message.value = 'ペアリングコードを発行しました。この画面を離れると再表示できません。'
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    pairing.value = false
  }
}

async function redeemPairingCode() {
  if (!pairRedeemForm.value.base_url.trim() || !pairRedeemForm.value.code.trim()) {
    error.value = '相手ノードのURLとペアリングコードを入力してください。'
    return
  }
  pairing.value = true
  try {
    const result = await api<{ node: StoredNode }>('/nodes/pairing/redeem', {
      method: 'POST',
      body: JSON.stringify({
        base_url: pairRedeemForm.value.base_url.trim(),
        code: pairRedeemForm.value.code.trim(),
        endpoints: [],
      }),
    })
    message.value = `${result.node.display_name} とペアリングしました。`
    error.value = ''
    pairRedeemForm.value = { base_url: '', code: '' }
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    pairing.value = false
  }
}

async function addGroupMember() {
  try {
    await api('/node-route-groups/member', {
      method: 'POST',
      body: JSON.stringify(groupForm.value),
    })
    message.value = `${groupForm.value.name} グループを更新しました。`
    error.value = ''
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}

onMounted(load)
</script>

<template>
  <section class="nodes-view">
    <div class="view-heading">
      <div>
        <h2>分散ノード</h2>
        <p>複数拠点の受信経路をまとめ、視聴・録画ごとに安定した通信経路を自動選択します。</p>
      </div>
      <button class="button secondary" :disabled="loading" @click="load">再読み込み</button>
    </div>

    <p v-if="error" class="notice error" role="alert" v-text="error" />
    <p v-if="message" class="notice" v-text="message" />

    <div v-if="data" class="local-node-card">
      <strong>このノード</strong>
      <span v-text="data.local.display_name" />
      <code v-text="data.local.node_id" />
    </div>

    <div class="node-grid">
      <section class="node-panel">
        <h3>ペアリング</h3>
        <p class="muted">
          片方でコードを発行し、もう片方でそのコードと相手のURLを入力すると、双方に共有クレデンシャルが保存されます。
          VPN や Tailscale に繋がっているだけでは認証になりません。
        </p>

        <label>用途メモ（任意）<input v-model="pairIssueForm.label" autocomplete="off" placeholder="東京の受信機" /></label>
        <button class="button" :disabled="pairing" @click="issuePairingCode">
          {{ pairing ? '処理中…' : 'ペアリングコードを発行' }}
        </button>

        <div v-if="issuedCode" class="pairing-code">
          <strong>この画面でしか表示されません</strong>
          <code class="pairing-code-value" v-text="issuedCode.code" />
          <span class="muted">
            有効期限 {{ Math.round(issuedCode.ttl_secs / 60) }} 分・1回限り<template v-if="issuedCode.node_listen_addr">
            / 相手が入力するURL: <code v-text="issuedCode.node_listen_addr" /></template>
          </span>
        </div>
        <ul v-if="data?.pending_pairings?.length" class="pending-list">
          <li v-for="(pending, idx) in data.pending_pairings" :key="idx">
            発行済みコード<template v-if="pending.label">（{{ pending.label }}）</template>
            — {{ formatExpiry(pending.expires_at_unix_ms) }}
          </li>
        </ul>

        <hr />

        <label>相手ノードのURL<input v-model="pairRedeemForm.base_url" autocomplete="off" placeholder="http://100.x.y.z:20773" /></label>
        <label>ペアリングコード<input v-model="pairRedeemForm.code" autocomplete="off" placeholder="ABCD-EF01-2345-6789" /></label>
        <button class="button" :disabled="pairing" @click="redeemPairingCode">
          {{ pairing ? '処理中…' : 'このコードでペアリング' }}
        </button>
      </section>

      <section class="node-panel">
        <h3>{{ form.node_id ? 'ノードを編集' : 'ノードを追加' }}</h3>
        <p class="muted">通常はペアリングを使ってください。ここは手動登録・編集用です。</p>
        <label>ノードID<input v-model="form.node_id" autocomplete="off" placeholder="tokyo" /></label>
        <label>表示名<input v-model="form.display_name" autocomplete="off" placeholder="東京" /></label>
        <label>受信拠点<input v-model="form.site_name" autocomplete="off" placeholder="東京都" /></label>
        <label>接続方式
          <select v-model="form.kind">
            <option v-for="kind in endpointKinds" :key="kind.value" :value="kind.value" v-text="kind.label" />
          </select>
        </label>
        <label>接続先<input v-model="form.address" autocomplete="off" placeholder="http://100.x.y.z:4512" /></label>
        <label class="check"><input v-model="form.auto_connect" type="checkbox" /> 自動接続</label>
        <label class="check"><input v-model="form.record_allowed" type="checkbox" /> 録画経路として使用可</label>
        <details>
          <summary>詳細設定</summary>
          <label>共有credential（64桁hex）<input v-model="form.credential" type="password" autocomplete="off" /></label>
        </details>
        <div class="actions">
          <button v-if="form.node_id" class="button secondary" @click="resetForm">新規入力</button>
          <button class="button" :disabled="saving" @click="save">{{ saving ? '保存中…' : '保存' }}</button>
        </div>
      </section>

      <section class="node-panel">
        <h3>Route Group</h3>
        <p class="muted">例: 群馬・栃木・茨城・東京を「関東」にまとめ、受信品質とネットワーク品質から自動分散します。</p>
        <label>グループ名<input v-model="groupForm.name" autocomplete="off" /></label>
        <label>ノード
          <select v-model="groupForm.node_id">
            <option v-for="entry in nodes" :key="entry.node.node_id" :value="entry.node.node_id" v-text="entry.node.display_name" />
          </select>
        </label>
        <label>重み<input v-model.number="groupForm.weight" type="number" min="1" max="10000" /></label>
        <button class="button" :disabled="!groupForm.node_id" @click="addGroupMember">グループへ追加</button>
        <div v-if="data?.route_groups.length" class="chips">
          <span v-for="group in data.route_groups" :key="group.id" class="chip" v-text="group.name" />
        </div>
      </section>
    </div>

    <section class="node-list">
      <h3>登録ノード</h3>
      <p v-if="!loading && !nodes.length" class="muted">登録されたリモートノードはありません。</p>
      <article v-for="entry in nodes" :key="entry.node.node_id" class="node-card">
        <div class="node-card-head">
          <div>
            <h4><span v-text="entry.node.display_name" /> <small v-if="entry.node.site_name" v-text="entry.node.site_name" /></h4>
            <code v-text="entry.node.node_id" />
          </div>
          <div class="actions compact">
            <span :class="['status-pill', entry.paired ? 'ok' : 'warn']">{{ entry.paired ? 'ペア済み' : '未ペア' }}</span>
            <span :class="['status-pill', entry.routable_routes > 0 ? 'ok' : 'warn']">
              受信可 {{ entry.routable_routes }}/{{ entry.total_routes }}
            </span>
            <button class="button secondary" @click="edit(entry)">編集</button>
            <button class="button" :disabled="probing === entry.node.node_id || !entry.paired" @click="probe(entry)">
              {{ probing === entry.node.node_id ? 'テスト中…' : '通信テスト' }}
            </button>
          </div>
        </div>

        <div class="endpoint-list">
          <div v-for="endpoint in entry.endpoints" :key="`${endpoint.kind}:${endpoint.address}`" class="endpoint-row">
            <strong v-text="endpoint.kind" /><code v-text="endpoint.address" />
            <span v-if="endpoint.record_allowed">RECORD可</span>
          </div>
        </div>

        <div v-if="probes[entry.node.node_id]" class="probe-result">
          <div class="selection-row">
            <span>VIEW <strong v-text="probes[entry.node.node_id].selected.view ?? '選択不可'" /></span>
            <span>PREVIEW <strong v-text="probes[entry.node.node_id].selected.preview ?? '選択不可'" /></span>
            <span>RECORD <strong v-text="probes[entry.node.node_id].selected.record ?? '選択不可'" /></span>
          </div>
          <div v-for="path in probes[entry.node.node_id].paths" :key="path.id" class="path-row">
            <strong v-text="pathLabel(path)" />
            <span>状態 {{ path.health.state }}</span>
            <span>RTT p95 {{ formatMs(path.health.rtt_p95_ms) }}</span>
            <span>帯域 p10 {{ formatMbps(path.health.throughput_down_p10_bps) }}</span>
            <span>信頼度 {{ Math.round(path.health.confidence * 100) }}%</span>
          </div>
        </div>
      </article>
    </section>
  </section>
</template>

<style scoped>
.nodes-view { display: grid; gap: 1.25rem; }
.view-heading, .node-card-head, .selection-row, .endpoint-row, .path-row { display: flex; gap: .8rem; align-items: center; justify-content: space-between; flex-wrap: wrap; }
.view-heading h2, .node-panel h3, .node-list h3, .node-card h4 { margin: 0; }
.view-heading p, .muted { color: var(--text-muted, #6b7280); }
.local-node-card, .node-panel, .node-card { border: 1px solid var(--border, #d9dee7); border-radius: 12px; padding: 1rem; background: var(--surface, rgba(255,255,255,.04)); }
.local-node-card { display: flex; gap: .75rem; align-items: center; flex-wrap: wrap; }
.node-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(300px, 1fr)); gap: 1rem; }
.node-panel { display: grid; gap: .75rem; align-content: start; }
.node-panel label { display: grid; gap: .35rem; font-weight: 600; }
.node-panel input, .node-panel select { width: 100%; box-sizing: border-box; padding: .6rem .7rem; border: 1px solid var(--border, #cbd5e1); border-radius: 8px; background: inherit; color: inherit; }
.node-panel .check { display: flex; align-items: center; gap: .5rem; }
.node-panel .check input { width: auto; }
.pairing-code { display: grid; gap: .4rem; padding: .75rem; border: 1px dashed var(--border, #cbd5e1); border-radius: 8px; }
/* The code must stay readable on a phone: wrap instead of scrolling the page. */
.pairing-code-value { font-size: 1.15rem; letter-spacing: .08em; font-weight: 700; overflow-wrap: anywhere; }
.pending-list { margin: 0; padding-left: 1.1rem; font-size: .85rem; opacity: .8; }
.node-panel hr { width: 100%; border: 0; border-top: 1px solid var(--border, #cbd5e1); margin: .25rem 0; }
.node-list { display: grid; gap: .75rem; }
.node-card { display: grid; gap: .9rem; }
.node-card small { font-weight: 400; opacity: .7; }
.actions { display: flex; gap: .5rem; justify-content: flex-end; flex-wrap: wrap; }
.actions.compact { align-items: center; }
.endpoint-list, .probe-result { display: grid; gap: .45rem; }
.endpoint-row, .path-row, .selection-row { justify-content: flex-start; padding: .55rem .7rem; border-radius: 8px; background: rgba(127,127,127,.08); }
.endpoint-row code { overflow-wrap: anywhere; }
.path-row span { min-width: 8rem; }
.status-pill, .chip { border-radius: 999px; padding: .2rem .55rem; font-size: .85rem; }
.status-pill.ok { background: rgba(34,197,94,.16); }
.status-pill.warn { background: rgba(245,158,11,.18); }
.chips { display: flex; gap: .4rem; flex-wrap: wrap; margin-top: .4rem; }
.chip { background: rgba(59,130,246,.14); }
code { font-size: .86em; }
@media (max-width: 720px) { .node-card-head { align-items: flex-start; } .actions.compact { justify-content: flex-start; } }
</style>

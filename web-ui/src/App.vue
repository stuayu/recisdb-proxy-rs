<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import OverviewView from './components/OverviewView.vue'
import BonDriversView from './components/BonDriversView.vue'
import ChannelsView from './components/ChannelsView.vue'
import GuideView from './components/GuideView.vue'
import ClientGuideView from './components/ClientGuideView.vue'
import SettingsView from './components/SettingsView.vue'
import AlertsView from './components/AlertsView.vue'
import ResourceView from './components/ResourceView.vue'
import SessionHistoryView from './components/SessionHistoryView.vue'
import EncodeProfilesView from './components/EncodeProfilesView.vue'
import { api, ApiError, setApiToken } from './api'

const tabs = [
  { id: 'overview', label: '概要', icon: '◫' },
  { id: 'bondrivers', label: 'BonDriver', icon: '▣' },
  { id: 'channels', label: 'チャンネル', icon: '⌁' },
  { id: 'guide', label: '番組表', icon: '▤' },
  { id: 'client-guide', label: 'クライアント設定', icon: '⚙' },
  { id: 'scan-history', label: 'スキャン履歴', icon: '↻' },
  { id: 'session-history', label: 'セッション履歴', icon: '◷' },
  { id: 'alerts', label: 'アラート', icon: '!' },
  { id: 'settings', label: '設定', icon: '◉' },
  { id: 'encode-profiles', label: 'エンコード', icon: '▶' },
]

const validTabs = new Set(tabs.map((tab) => tab.id))
const hashTab = location.hash.slice(1)
const active = ref(validTabs.has(hashTab) ? hashTab : 'overview')
const dark = ref(localStorage.getItem('dashboardTheme') === 'dark')
const token = ref(localStorage.getItem('recisdbApiToken') || '')
const tokenOpen = ref(false)
const connection = ref<'checking' | 'connected' | 'error'>('checking')
let connectionTimer = 0

const serverVersion = ref<string | null>(null)

interface UpdateReleaseInfo {
  tag: string
  url: string
  published_at: string | null
}

interface UpdateCheckResponse {
  current_version: string
  stable: UpdateReleaseInfo | null
  prerelease: UpdateReleaseInfo | null
  self_update_supported: boolean
}

type UpdateKind = 'stable' | 'prerelease'
type UpdateApplyState = 'idle' | 'downloading' | 'extracting' | 'replacing' | 'restarting' | 'error'

interface UpdateStatusResponse {
  state: UpdateApplyState
  message: string | null
}

const updateCheck = ref<UpdateCheckResponse | null>(null)

const DISMISSED_UPDATE_TAGS_KEY = 'dismissedUpdateTags'

function loadDismissedUpdateTags(): Partial<Record<UpdateKind, string>> {
  try {
    const raw = localStorage.getItem(DISMISSED_UPDATE_TAGS_KEY)
    if (!raw) return {}
    const parsed = JSON.parse(raw) as Partial<Record<UpdateKind, string>>
    return parsed && typeof parsed === 'object' ? parsed : {}
  } catch {
    return {}
  }
}

const dismissedUpdateTags = ref<Partial<Record<UpdateKind, string>>>(loadDismissedUpdateTags())

const visibleStableUpdate = computed(() => {
  const info = updateCheck.value?.stable
  if (!info) return null
  return dismissedUpdateTags.value.stable === info.tag ? null : info
})

const visiblePrereleaseUpdate = computed(() => {
  const info = updateCheck.value?.prerelease
  if (!info) return null
  return dismissedUpdateTags.value.prerelease === info.tag ? null : info
})

/** Apply-dialog state machine. `null` means the dialog is closed. */
interface ApplyDialogState {
  kind: UpdateKind
  tag: string
  phase: 'confirm' | 'running' | 'reconnecting' | 'error'
  status: UpdateApplyState
  statusMessage: string | null
  reconnectTimedOut: boolean
}
const applyDialog = ref<ApplyDialogState | null>(null)
let statusPollTimer = 0
let reconnectPollTimer = 0
let reconnectDeadline = 0

const UPDATE_STATE_LABELS: Record<UpdateApplyState, string> = {
  idle: '待機中',
  downloading: 'ダウンロード中',
  extracting: '展開中',
  replacing: '置換中',
  restarting: '再起動中',
  error: 'エラー',
}

function select(id: string) {
  active.value = id
  location.hash = id
  requestAnimationFrame(() => document.getElementById('main')?.focus())
}

function toggleTheme() {
  dark.value = !dark.value
  localStorage.setItem('dashboardTheme', dark.value ? 'dark' : 'light')
}

function saveToken() {
  setApiToken(token.value)
  tokenOpen.value = false
  location.reload()
}

function syncHash() {
  const id = location.hash.slice(1)
  active.value = validTabs.has(id) ? id : 'overview'
}
async function checkConnection() {
  try {
    await api('/stats')
    connection.value = 'connected'
  } catch {
    connection.value = 'error'
  }
}

function onKeydown(event: KeyboardEvent) {
  if (event.key === 'Escape') tokenOpen.value = false
}

async function fetchServerVersion(): Promise<string | null> {
  try {
    const data = await api<{ version: string }>('/version')
    return data.version
  } catch {
    return null
  }
}

async function checkForUpdate() {
  try {
    updateCheck.value = await api<UpdateCheckResponse>('/update/check')
  } catch (error) {
    // Update checks are best-effort — a transient failure just means no
    // notice is shown; the periodic caller (server-side, 6h) retries later.
    console.debug('update check failed', error)
  }
}

function persistDismissedUpdateTags() {
  localStorage.setItem(DISMISSED_UPDATE_TAGS_KEY, JSON.stringify(dismissedUpdateTags.value))
}

function dismissUpdate(kind: UpdateKind) {
  const info = kind === 'stable' ? updateCheck.value?.stable : updateCheck.value?.prerelease
  if (!info) return
  dismissedUpdateTags.value = { ...dismissedUpdateTags.value, [kind]: info.tag }
  persistDismissedUpdateTags()
}

function stopStatusPoll() {
  if (statusPollTimer) {
    window.clearInterval(statusPollTimer)
    statusPollTimer = 0
  }
}

function stopReconnectPoll() {
  if (reconnectPollTimer) {
    window.clearInterval(reconnectPollTimer)
    reconnectPollTimer = 0
  }
}

function openApplyDialog(kind: UpdateKind, tag: string) {
  applyDialog.value = {
    kind,
    tag,
    phase: 'confirm',
    status: 'idle',
    statusMessage: null,
    reconnectTimedOut: false,
  }
}

function closeApplyDialog() {
  stopStatusPoll()
  stopReconnectPoll()
  applyDialog.value = null
}

/** Backdrop clicks only dismiss the dialog while it's safe to do so — not
 * mid-update, where losing sight of progress could confuse the operator. */
function onApplyDialogBackdropClick() {
  const phase = applyDialog.value?.phase
  if (phase === 'confirm' || phase === 'error' || applyDialog.value?.reconnectTimedOut) {
    closeApplyDialog()
  }
}

function beginReconnectPoll() {
  if (!applyDialog.value) return
  applyDialog.value.phase = 'reconnecting'
  applyDialog.value.reconnectTimedOut = false
  reconnectDeadline = Date.now() + 60_000
  reconnectPollTimer = window.setInterval(async () => {
    const version = await fetchServerVersion()
    if (version) {
      stopReconnectPoll()
      location.reload()
      return
    }
    if (Date.now() >= reconnectDeadline) {
      stopReconnectPoll()
      if (applyDialog.value) applyDialog.value.reconnectTimedOut = true
    }
  }, 2000)
}

function beginStatusPoll() {
  statusPollTimer = window.setInterval(async () => {
    if (!applyDialog.value) {
      stopStatusPoll()
      return
    }
    try {
      const status = await api<UpdateStatusResponse>('/update/status')
      if (!applyDialog.value) return
      applyDialog.value.status = status.state
      applyDialog.value.statusMessage = status.message
      if (status.state === 'error') {
        stopStatusPoll()
        applyDialog.value.phase = 'error'
      } else if (status.state === 'restarting' || status.state === 'idle') {
        // apply_update sets the server state to `downloading` before it
        // even returns, so observing `idle` here means the restarted
        // process is already answering — the 1-second `restarting` window
        // can easily fall between two polls.
        stopStatusPoll()
        beginReconnectPoll()
      }
    } catch (error) {
      // Transient poll failures are expected once the server starts
      // shutting down for the restart — keep polling until it errors out
      // via the state machine or the dialog is closed.
      console.debug('update status poll failed', error)
    }
  }, 1000)
}

async function startApply() {
  const dialog = applyDialog.value
  if (!dialog) return
  dialog.phase = 'running'
  dialog.status = 'idle'
  dialog.statusMessage = null
  try {
    await api('/update/apply', {
      method: 'POST',
      body: JSON.stringify({ tag: dialog.tag }),
    })
    beginStatusPoll()
  } catch (error) {
    dialog.phase = 'error'
    dialog.statusMessage =
      error instanceof ApiError ? error.message : '更新の開始に失敗しました。'
  }
}

const applyStatusLabel = computed(() => {
  const dialog = applyDialog.value
  if (!dialog) return ''
  return UPDATE_STATE_LABELS[dialog.status] ?? dialog.status
})

onMounted(() => {
  window.addEventListener('hashchange', syncHash)
  window.addEventListener('keydown', onKeydown)
  void checkConnection()
  connectionTimer = window.setInterval(checkConnection, 5000)
  void fetchServerVersion().then((version) => {
    serverVersion.value = version
  })
  void checkForUpdate()
})

onUnmounted(() => {
  window.removeEventListener('hashchange', syncHash)
  window.removeEventListener('keydown', onKeydown)
  window.clearInterval(connectionTimer)
  stopStatusPoll()
  stopReconnectPoll()
})
</script>

<template>
  <div class="app" :class="{ dark }">
    <a class="skip-link" href="#main">本文へ移動</a>
    <header class="topbar">
      <div>
        <h1>
          recisdb-proxy
          <span v-if="serverVersion" class="app-version" v-text="`v${serverVersion}`" />
        </h1>
        <p>TVプロキシサーバー 管理コンソール</p>
        <p
          class="connection-status"
          :class="connection"
          v-text="
            connection === 'connected'
              ? '● サーバー接続中'
              : connection === 'error'
                ? '● 接続を確認できません'
                : '● 接続確認中'
          "
        />
      </div>
      <div v-if="visibleStableUpdate || visiblePrereleaseUpdate" class="update-notices">
        <div v-if="visibleStableUpdate" class="update-notice">
          <span v-text="`新しいバージョン ${visibleStableUpdate.tag}`" />
          <a :href="visibleStableUpdate.url" target="_blank" rel="noopener">リリースページ</a>
          <button
            v-if="updateCheck?.self_update_supported"
            class="update-notice-apply"
            @click="openApplyDialog('stable', visibleStableUpdate.tag)"
            v-text="'更新'"
          />
          <button
            class="update-notice-close"
            aria-label="更新通知を閉じる"
            @click="dismissUpdate('stable')"
            v-text="'×'"
          />
        </div>
        <div v-if="visiblePrereleaseUpdate" class="update-notice prerelease">
          <span class="update-notice__badge" v-text="'プレリリース'" />
          <span v-text="`新しいバージョン ${visiblePrereleaseUpdate.tag}`" />
          <a :href="visiblePrereleaseUpdate.url" target="_blank" rel="noopener">リリースページ</a>
          <button
            v-if="updateCheck?.self_update_supported"
            class="update-notice-apply"
            @click="openApplyDialog('prerelease', visiblePrereleaseUpdate.tag)"
            v-text="'更新'"
          />
          <button
            class="update-notice-close"
            aria-label="更新通知を閉じる"
            @click="dismissUpdate('prerelease')"
            v-text="'×'"
          />
        </div>
      </div>
      <div class="top-actions">
        <button class="icon-button" aria-label="APIトークン" @click="tokenOpen = true">鍵</button>
        <button
          class="icon-button"
          aria-label="テーマ切替"
          @click="toggleTheme"
          v-text="dark ? '☀' : '☾'"
        />
      </div>
    </header>
    <div class="layout">
      <nav class="nav" aria-label="メインメニュー">
        <button
          v-for="tab in tabs"
          :key="tab.id"
          :class="['nav-item', { active: active === tab.id }]"
          :aria-current="active === tab.id ? 'page' : undefined"
          @click="select(tab.id)"
        >
          <span aria-hidden="true" v-text="tab.icon" />
          <span v-text="tab.label" />
        </button>
      </nav>
      <main id="main" tabindex="-1">
        <OverviewView v-if="active === 'overview'" />
        <BonDriversView v-else-if="active === 'bondrivers'" />
        <ChannelsView v-else-if="active === 'channels'" />
        <GuideView v-else-if="active === 'guide'" />
        <ClientGuideView v-else-if="active === 'client-guide'" />
        <ResourceView
          v-else-if="active === 'scan-history'"
          title="スキャン履歴"
          endpoint="/scan-history"
          :keys="['history', 'scans', 'data']"
          :columns="['scan_time', 'bon_driver_id', 'channel_count', 'success', 'error_message']"
          storage-key="columns:scan-history"
        />
        <SessionHistoryView v-else-if="active === 'session-history'" />
        <AlertsView v-else-if="active === 'alerts'" />
        <SettingsView v-else-if="active === 'settings'" />
        <EncodeProfilesView v-else />
      </main>
    </div>
    <div v-if="tokenOpen" class="dialog-backdrop" @click.self="tokenOpen = false">
      <section class="dialog" role="dialog" aria-modal="true" aria-labelledby="token-title">
        <h2 id="token-title">APIトークン</h2>
        <p>認証が有効な場合のBearerトークンを保存します。</p>
        <input v-model="token" type="password" autocomplete="off" @keyup.enter="saveToken" />
        <div class="actions">
          <button class="button secondary" @click="tokenOpen = false">キャンセル</button>
          <button class="button" @click="saveToken">保存</button>
        </div>
      </section>
    </div>
    <div v-if="applyDialog" class="dialog-backdrop" @click.self="onApplyDialogBackdropClick">
      <section class="dialog" role="dialog" aria-modal="true" aria-labelledby="apply-update-title">
        <h2 id="apply-update-title" v-text="`recisdb-proxy を更新 (${applyDialog.tag})`" />
        <template v-if="applyDialog.phase === 'confirm'">
          <p
            v-text="
              `recisdb-proxy を ${applyDialog.tag} に更新してサーバーを再起動します。接続中のクライアントは一時切断されます。`
            "
          />
          <p>BonDriver_NetworkProxy.dll は自動更新されません。必要ならリリースページから手動で更新してください。</p>
          <div class="actions">
            <button class="button secondary" @click="closeApplyDialog">キャンセル</button>
            <button class="button" @click="startApply">更新を実行</button>
          </div>
        </template>
        <template v-else-if="applyDialog.phase === 'running'">
          <p v-text="applyStatusLabel" />
          <p v-if="applyDialog.statusMessage" class="notice" v-text="applyDialog.statusMessage" />
        </template>
        <template v-else-if="applyDialog.phase === 'reconnecting'">
          <p v-if="!applyDialog.reconnectTimedOut">
            サーバーを再起動しています。しばらくお待ちください…
          </p>
          <p v-else class="notice error">
            自動再接続できませんでした。手動でページを再読み込みしてください。
          </p>
          <div v-if="applyDialog.reconnectTimedOut" class="actions">
            <button class="button" @click="closeApplyDialog">閉じる</button>
          </div>
        </template>
        <template v-else-if="applyDialog.phase === 'error'">
          <p
            class="notice error"
            role="alert"
            v-text="applyDialog.statusMessage || '更新に失敗しました。'"
          />
          <div class="actions">
            <button class="button" @click="closeApplyDialog">閉じる</button>
          </div>
        </template>
      </section>
    </div>
  </div>
</template>

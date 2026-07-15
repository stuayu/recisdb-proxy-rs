<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue'
import OverviewView from './components/OverviewView.vue'
import BonDriversView from './components/BonDriversView.vue'
import ChannelsView from './components/ChannelsView.vue'
import ClientGuideView from './components/ClientGuideView.vue'
import SettingsView from './components/SettingsView.vue'
import AlertsView from './components/AlertsView.vue'
import ResourceView from './components/ResourceView.vue'
import SessionHistoryView from './components/SessionHistoryView.vue'
import EncodeProfilesView from './components/EncodeProfilesView.vue'
import { api, setApiToken } from './api'

const tabs = [
  { id: 'overview', label: '概要', icon: '◫' },
  { id: 'bondrivers', label: 'BonDriver', icon: '▣' },
  { id: 'channels', label: 'チャンネル', icon: '⌁' },
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

onMounted(() => {
  window.addEventListener('hashchange', syncHash)
  window.addEventListener('keydown', onKeydown)
  void checkConnection()
  connectionTimer = window.setInterval(checkConnection, 5000)
})

onUnmounted(() => {
  window.removeEventListener('hashchange', syncHash)
  window.removeEventListener('keydown', onKeydown)
  window.clearInterval(connectionTimer)
})
</script>

<template>
  <div class="app" :class="{ dark }">
    <a class="skip-link" href="#main">本文へ移動</a>
    <header class="topbar">
      <div>
        <h1>recisdb-proxy</h1>
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
        <ClientGuideView v-else-if="active === 'client-guide'" />
        <ResourceView
          v-else-if="active === 'scan-history'"
          title="スキャン履歴"
          endpoint="/scan-history"
          :keys="['history', 'scans', 'data']"
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
  </div>
</template>

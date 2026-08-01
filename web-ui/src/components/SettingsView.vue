<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, type JsonRecord } from '../api'

const endpoints = [
  ['スキャンスケジューラー', '/scan-config'],
  ['チューナー最適化', '/tuner-config'],
  ['BNDP外部エンコード（tsreplace）', '/tsreplace-config'],
  ['ブラウザプレビュー', '/preview-config'],
] as const
const selected = ref<string>(endpoints[0][1])
const config = ref<JsonRecord>({})
const message = ref('')
const error = ref('')
const label = computed(() => endpoints.find((item) => item[1] === selected.value)?.[0] ?? '設定')
const entries = computed(() => Object.entries(config.value))
const protectedKeys = new Set(['command_path', 'preprocessor_path'])
function displayName(key: string) {
  const names: Record<string, string> = {
    check_interval_secs: '確認間隔（秒）',
    max_concurrent_scans: '同時スキャン数',
    scan_timeout_secs: 'スキャンタイムアウト（秒）',
    signal_lock_wait_ms: '信号ロック待機（ms）',
    ts_read_timeout_ms: 'TS読み取りタイムアウト（ms）',
    keep_alive_secs: 'チューナー維持時間（秒）',
    prewarm_enabled: 'プリウォームを有効化',
    prewarm_timeout_secs: 'プリウォーム時間（秒）',
    set_channel_retry_interval_ms: '選局再試行間隔（ms）',
    set_channel_retry_timeout_ms: '選局再試行タイムアウト（ms）',
    signal_poll_interval_ms: '信号ポーリング間隔（ms）',
    signal_wait_timeout_ms: '信号待機タイムアウト（ms）',
    prefill_view_ms: '視聴プリフィル（ms）',
    prefill_preview_ms: 'プレビュープリフィル（ms）',
    prefill_record_ms: '録画プリフィル（ms）',
    jitter_safety_factor: 'ジッター安全係数',
    enabled: '有効にする',
    arguments: 'エンコーダー引数',
    read_timeout_ms: '読み取りタイムアウト（ms）',
    passthrough_on_error: '失敗時は無変換で継続',
    max_concurrent_encoders: '同時エンコード数',
    preprocessor_arguments: '前処理プログラム引数',
    command_path: '実行ファイル（TOMLで設定）',
    preprocessor_path: '前処理プログラム（TOMLで設定）',
  }
  return names[key] ?? key
}
async function load() {
  try {
    const response = await api<JsonRecord>(selected.value)
    const value = response.config
    config.value =
      value && typeof value === 'object' && !Array.isArray(value)
        ? { ...(value as JsonRecord) }
        : { ...response }
    message.value = ''
    error.value = ''
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}
async function save() {
  try {
    const payload = Object.fromEntries(entries.value.filter(([key]) => !protectedKeys.has(key)))
    await api(selected.value, { method: 'POST', body: JSON.stringify(payload) })
    message.value = `${label.value}を保存しました。`
    error.value = ''
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}
// --- サーバー(OSサービス)状態と再起動 -------------------------------
type ServiceStatus = {
  supported: boolean
  manager: string
  name: string
  scope: string
  installed: boolean
  running: boolean
  enabled: boolean
  detail: string | null
}
type ServiceStatusResponse = {
  supported: boolean
  running_under_service_manager: boolean
  restart_method: string
  service: ServiceStatus
}

const service = ref<ServiceStatusResponse | null>(null)
const serviceError = ref('')
const restarting = ref(false)
const restartMessage = ref('')

const restartMethodLabel: Record<string, string> = {
  service_manager_respawn: 'サービスマネージャーによる自動再起動',
  service_control_manager: 'Windowsサービスの停止→開始',
  exec_self: 'プロセスの再実行',
}
const scopeLabel: Record<string, string> = { system: 'システム', user: 'ユーザー' }

async function loadService() {
  try {
    service.value = await api<ServiceStatusResponse>('/service/status')
    serviceError.value = ''
  } catch (cause) {
    serviceError.value = cause instanceof Error ? cause.message : String(cause)
  }
}

/** サーバーが再び応答するまで待つ(再起動には数秒かかる)。 */
async function waitForServer(timeoutMs = 60000) {
  const deadline = Date.now() + timeoutMs
  // 停止し切る前に成功と判定しないよう、まず少し待つ。
  await new Promise((resolve) => setTimeout(resolve, 3000))
  while (Date.now() < deadline) {
    try {
      await api('/version')
      return true
    } catch {
      await new Promise((resolve) => setTimeout(resolve, 2000))
    }
  }
  return false
}

async function restartServer() {
  const ok = window.confirm(
    'recisdb-proxy を再起動します。視聴中・録画中のセッションはすべて切断されます。よろしいですか？',
  )
  if (!ok) return
  restarting.value = true
  restartMessage.value = '再起動を要求しました。サーバーの復帰を待っています…'
  try {
    await api('/service/restart', { method: 'POST' })
  } catch (cause) {
    restarting.value = false
    restartMessage.value = ''
    serviceError.value = cause instanceof Error ? cause.message : String(cause)
    return
  }
  const back = await waitForServer()
  restarting.value = false
  restartMessage.value = back
    ? 'サーバーが再起動しました。'
    : 'サーバーが時間内に応答しませんでした。手動での起動が必要かもしれません。'
  if (back) await loadService()
}

function updateNumber(key: string, event: Event) {
  config.value[key] = Number((event.target as HTMLInputElement).value)
}
onMounted(() => {
  void load()
  void loadService()
})
</script>
<template>
  <section class="view">
    <div class="view-heading">
      <div>
        <h2>設定</h2>
        <p>
          旧ダッシュボードのスキャン・チューナー・BNDPエンコード・ブラウザプレビュー設定を編集します。
        </p>
      </div>
      <div class="actions">
        <button class="button secondary" @click="load">再読み込み</button
        ><button class="button" @click="save">保存</button>
      </div>
    </div>
    <label class="field"
      ><span>設定カテゴリ</span
      ><select v-model="selected" @change="load">
        <option
          v-for="item in endpoints"
          :key="item[1]"
          :value="item[1]"
          v-text="item[0]"
        /></select
    ></label>
    <div class="panel">
      <h3>サーバー</h3>
      <p v-if="serviceError" class="notice error" role="alert" v-text="serviceError" />
      <dl v-if="service" class="service-status">
        <dt>サービス登録</dt>
        <dd v-if="service.service.installed">
          {{ service.service.name }}（{{
            scopeLabel[service.service.scope] ?? service.service.scope
          }}
          / {{ service.service.manager }}）— {{ service.service.running ? '稼働中' : '停止中' }}、
          自動起動{{ service.service.enabled ? '有効' : '無効' }}
        </dd>
        <dd v-else>
          未登録（セットアップウィザード、または <code>recisdb-proxy service install</code>
          で登録できます）
        </dd>
        <dt>再起動方式</dt>
        <dd v-text="restartMethodLabel[service.restart_method] ?? service.restart_method" />
      </dl>
      <p v-if="service && !service.running_under_service_manager" class="muted">
        現在サービス管理下では動作していません。再起動すると同じ引数でプロセスを起動し直します。
      </p>
      <p v-if="restartMessage" class="notice" v-text="restartMessage" />
      <button class="button secondary" type="button" :disabled="restarting" @click="restartServer">
        {{ restarting ? '再起動中…' : 'サーバーを再起動' }}
      </button>
    </div>
    <p v-if="message" class="notice success" v-text="message" />
    <p v-if="error" class="notice error" role="alert" v-text="error" />
    <form class="panel settings-form" @submit.prevent="save">
      <h3 v-text="label" />
      <template v-for="[key, value] in entries" :key="key">
        <label v-if="typeof value === 'boolean'" class="check"
          ><input v-model="config[key]" type="checkbox" :disabled="protectedKeys.has(key)" /><span
            v-text="displayName(key)"
        /></label>
        <label v-else class="field"
          ><span v-text="displayName(key)" /><input
            v-if="typeof value === 'number'"
            :value="Number(value)"
            :disabled="protectedKeys.has(key)"
            type="number"
            step="any"
            @input="updateNumber(key, $event)" /><input
            v-else
            v-model="config[key]"
            :disabled="protectedKeys.has(key)"
            type="text"
        /></label>
      </template>
      <p class="muted">
        実行ファイルのパスは安全上の理由で画面から変更できません。recisdb-proxy.tomlで設定してください。
      </p>
      <button class="button" type="submit">保存</button>
    </form>
  </section>
</template>

<style scoped>
.service-status {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 4px 16px;
  margin: 0 0 12px;
}

.service-status dt {
  font-weight: 600;
}

.service-status dd {
  margin: 0;
}
</style>

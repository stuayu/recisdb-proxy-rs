<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, type JsonRecord } from '../api'

const endpoints = [
  ['EPG自動取得', '/settings/epg'],
  ['スキャンスケジューラー', '/scan-config'],
  ['チューナー最適化', '/tuner-config'],
  ['BNDP外部エンコード（tsreplace）', '/tsreplace-config'],
  ['ブラウザプレビュー', '/preview-config'],
  ['ログ出力', '/log-config'],
] as const
const selected = ref<string>(endpoints[0][1])
const config = ref<JsonRecord>({})
const message = ref('')
const error = ref('')
const label = computed(() => endpoints.find((item) => item[1] === selected.value)?.[0] ?? '設定')
const isEpg = computed(() => selected.value === '/settings/epg')
const epgPresets = ref<JsonRecord[]>([])
const selectedEpgPreset = ref<number | null>(null)
const epgStatus = ref<JsonRecord | null>(null)
const editingPreset = ref<JsonRecord | null>(null)
const creatingPreset = ref(false)
const presetGroups = [
  { label: '基本', keys: ['enabled', 'target_refresh_secs', 'max_stale_secs', 'min_future_coverage_hours', 'target_future_coverage_hours'] },
  { label: 'チューナー', keys: ['reserve_tuners', 'prefer_local', 'preemptible'] },
  { label: '負荷', keys: ['cpu_soft_limit_percent', 'cpu_hard_limit_percent'] },
  { label: 'リモート', keys: ['allow_remote', 'remote_prefer_metadata_execution', 'remote_allow_ts_transport'] },
  { label: '詳細', keys: ['min_dwell_secs', 'normal_dwell_secs', 'max_dwell_secs', 'idle_section_timeout_secs'] },
] as const
const presetLabels: Record<string, string> = {
  enabled: '有効', target_refresh_secs: '更新間隔(秒)', max_stale_secs: '最大古さ(秒)',
  min_future_coverage_hours: '最低coverage(時間)', target_future_coverage_hours: '目標coverage(時間)',
  reserve_tuners: 'チューナー確保', prefer_local: 'ローカル優先', preemptible: '録画・視聴で中断',
  cpu_soft_limit_percent: 'CPU延期上限(%)', cpu_hard_limit_percent: 'CPU中断上限(%)', allow_remote: 'リモート許可',
  remote_prefer_metadata_execution: 'リモート解析優先', remote_allow_ts_transport: 'TS転送許可',
  min_dwell_secs: '最小滞在(秒)', normal_dwell_secs: '通常滞在(秒)', max_dwell_secs: '最大滞在(秒)', idle_section_timeout_secs: '無受信タイムアウト(秒)',
}
const presetSettingKeys = presetGroups.flatMap((group) => [...group.keys])
const epgReasonLabels: Record<string, string> = {
  scheduled: '取得を開始しました', scan_failed: 'EPG取得に失敗しました',
  disabled: 'EPG自動取得が無効です', not_due: '更新時刻にはまだ達していません',
  no_tuner_available: '利用可能なチューナーがありません', cpu_soft_limit: 'CPU負荷が設定値を超過しました',
  cpu_hard_limit: 'CPU負荷が高いため取得を中断しました', backoff: '前回の失敗後の待機中です',
  preempted_by_record: '録画を優先して取得を延期しました', preempted_by_view: '視聴を優先して取得を延期しました',
  no_compatible_tuner: '対応する受信帯域のチューナーがありません',
}
function epgReasonLabel(value: unknown): string {
  if (value && typeof value === 'object') {
    const reason = value as JsonRecord
    const code = String(reason.code ?? '')
    return epgReasonLabels[code] ?? '取得理由を確認できません（未知の理由コード）'
  }
  return value == null ? '—' : '取得理由を確認できません（不正な理由データ）'
}
function epgReasonNetworkText(value: unknown): string {
  if (!value || typeof value !== 'object') return '系統を確認できません'
  const reason = value as JsonRecord
  const networkId = Number(reason.networkId)
  const tsid = Number(reason.tsid)
  const label = typeof reason.label === 'string' && reason.label.length > 0
    ? reason.label
    : `NID ${networkId} / TSID ${tsid}`
  const tuner = reason.tunerId !== null && reason.tunerId !== undefined ? `チューナー#${reason.tunerId}` : ''
  const node = typeof reason.nodeId === 'string' && reason.nodeId ? reason.nodeId : ''
  const source = [tuner, node].filter(Boolean).join(' / ')
  return `${label} (${networkId}/${tsid})${source ? ` ${source}` : ''}`
}
const epgReasonList = computed(() => {
  const values = epgStatus.value?.reasons
  return Array.isArray(values) ? values : epgStatus.value?.reason ? [epgStatus.value.reason] : []
})
const epgReasonGroups = computed(() => {
  const groups = new Map<string, { code: string; count: number; reasons: unknown[] }>()
  for (const reason of epgReasonList.value) {
    const code = reason && typeof reason === 'object' ? String((reason as JsonRecord).code ?? '') : ''
    const group = groups.get(code) ?? { code, count: 0, reasons: [] }
    group.count = Math.max(group.count, Number((reason as JsonRecord)?.count) || 0)
    group.reasons.push(reason)
    groups.set(code, group)
  }
  return [...groups.values()]
})
function formatEpoch(value: unknown): string {
  return typeof value === 'number'
    ? new Date(value * 1000).toLocaleString('ja-JP', { dateStyle: 'short', timeStyle: 'short' })
    : '—'
}
function epgStateValue(key: string): unknown {
  const state = epgStatus.value?.state
  return state && typeof state === 'object' ? (state as JsonRecord)[key] : null
}
const entries = computed(() => Object.entries(config.value))
const protectedKeys = new Set([
  'command_path',
  'preprocessor_path',
  'env_override',
  'effective_level',
])
const LOG_LEVELS = ['trace', 'debug', 'info', 'warn', 'error'] as const
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
    min_hold_secs: 'チューナー最低保持時間（秒）',
    reject_cooldown_ms: '選局拒否クールダウン（ms）',
    no_data_timeout_secs: '無データ回収タイムアウト（秒）',
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
    level: 'ログレベル',
    retention_days: 'ログ保持日数（日）',
    effective_level: '現在適用中のレベル（変更不可）',
    env_override: 'RUST_LOG による上書き（変更不可）',
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
    if (isEpg.value) {
      const presets = await api<JsonRecord>('/epg-presets')
      epgPresets.value = Array.isArray(presets.presets) ? (presets.presets as JsonRecord[]) : []
      selectedEpgPreset.value = typeof config.value.selected_preset_id === 'number' ? Number(config.value.selected_preset_id) : null
      epgStatus.value = await api<JsonRecord>('/epg/status')
    }
    message.value = ''
    error.value = ''
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}
async function save() {
  try {
    if (isEpg.value) config.value.selected_preset_id = selectedEpgPreset.value
    const payload = Object.fromEntries(entries.value.filter(([key]) => !protectedKeys.has(key)))
    await api(selected.value, {
      method: isEpg.value ? 'PUT' : 'POST',
      body: JSON.stringify(isEpg.value ? config.value : payload),
    })
    message.value = `${label.value}を保存しました。`
    error.value = ''
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}
async function duplicateEpgPreset(preset: JsonRecord) {
  try {
    const copy = Object.fromEntries(presetSettingKeys.map((key) => [key, preset[key] ?? null]))
    await api('/epg-presets', { method: 'POST', body: JSON.stringify({
      name: `${String(preset.name)}（コピー）`,
      description: preset.description ?? '',
      ...copy,
    }) })
    await load()
    message.value = 'プリセットを複製しました。'
  } catch (cause) { error.value = cause instanceof Error ? cause.message : String(cause) }
}
function editEpgPreset(preset: JsonRecord) {
  if (preset.is_system) return
  editingPreset.value = { ...preset }
}
function startCreateEpgPreset() {
  creatingPreset.value = true
  editingPreset.value = {
    name: '', description: '', enabled: true,
    ...Object.fromEntries(presetSettingKeys.filter((key) => key !== 'enabled').map((key) => [key, null])),
  }
}
function clearPresetValue(key: string) {
  if (editingPreset.value) editingPreset.value[key] = null
}
function updatePresetText(key: string, event: Event) {
  if (editingPreset.value) editingPreset.value[key] = (event.target as HTMLTextAreaElement).value
}
function updatePresetNumber(key: string, event: Event) {
  if (!editingPreset.value) return
  const value = (event.target as HTMLInputElement).value.trim()
  editingPreset.value[key] = value === '' ? null : Number(value)
}
function presetValue(value: unknown, unit: 'seconds' | 'hours' | 'percent' | 'boolean'): string {
  if (value === null || value === undefined) return '全体設定'
  if (unit === 'boolean') return value ? 'はい' : 'いいえ'
  const number = Number(value)
  if (!Number.isFinite(number)) return '—'
  if (unit === 'percent') return `${number}%`
  if (unit === 'hours') {
    if (number % 24 === 0) return `${number / 24}日`
    return `${number}時間`
  }
  if (number >= 86400 && number % 86400 === 0) return `約${number / 86400}日`
  if (number >= 3600 && number % 3600 === 0) return `約${number / 3600}時間`
  if (number >= 60 && number % 60 === 0) return `約${number / 60}分`
  return `約${number}秒`
}
function presetPlaceholder(key: string): string {
  const examples: Record<string, string> = {
    target_refresh_secs: '全体設定の値(例: 21600)',
    target_future_coverage_hours: '全体設定の値(例: 168)',
  }
  return examples[key] ?? '全体設定の値'
}
async function saveEpgPreset() {
  if (!editingPreset.value) return
  try {
    const isNew = creatingPreset.value
    if (!String(editingPreset.value.name ?? '').trim()) { error.value = 'プリセット名を入力してください'; return }
    await api(isNew ? '/epg-presets' : `/epg-presets/${String(editingPreset.value.id)}`, { method: isNew ? 'POST' : 'PUT', body: JSON.stringify(editingPreset.value) })
    creatingPreset.value = false
    editingPreset.value = null
    await load()
    message.value = 'プリセットを保存しました。'
  } catch (cause) { error.value = cause instanceof Error ? cause.message : String(cause) }
}
async function deleteEpgPreset(preset: JsonRecord) {
  if (preset.is_system || !window.confirm(`「${String(preset.name)}」を削除しますか？`)) return
  try {
    await api(`/epg-presets/${String(preset.id)}`, { method: 'DELETE' })
    await load()
    message.value = 'プリセットを削除しました。'
  } catch (cause) { error.value = cause instanceof Error ? cause.message : String(cause) }
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

// --- ブラウザプレビューの自動セットアップ ----------------------------
// エンコーダ(ffmpeg)と前段処理(tsreadex)を検出、無ければ取得して有効化する。
// 実行ファイルのパスはサーバー側の検出結果しか使わないため、ここからは何も送らない。
type PreviewSetupReport = {
  enabled: boolean
  encoder_path: string
  encoder_source: string
  video_encoder: string
  preprocessor_path: string
  warnings: string[]
}

// ---- 開発版 (GitHub Actions のアーティファクト) への更新 ----
//
// アーティファクトのダウンロードは公開リポジトリでも認証が要るため、
// トークンが無いと一覧は見えても更新できない。その状態を先に見せる。
type DevBuild = {
  run_id: number
  branch: string | null
  sha: string | null
  title: string | null
  created_at: string | null
  html_url: string | null
  artifact_id: number | null
  size_in_bytes: number | null
  installable: boolean
}
type DevBuildsResponse = {
  success: boolean
  supported: boolean
  token_configured: boolean
  reason?: string
  error?: string
  artifact_name?: string
  builds: DevBuild[]
}

const devBuilds = ref<DevBuildsResponse | null>(null)
const devBuildsLoading = ref(false)
const devError = ref('')
const devMessage = ref('')
const githubToken = ref('')
const tokenConfigured = ref(false)
const devApplying = ref<number | null>(null)
const devStatus = ref('')

/// 保存済みかどうかだけを問い合わせる（値はサーバーが返さない）。
///
/// 一覧の取得と分けているのは、`/update/dev-builds` が GitHub API を
/// 「ラン一覧 + ラン毎の成果物」で十数回叩くため。未認証の GitHub API は
/// 1時間60回しかないので、設定画面を開くたびに消費させない。
async function loadTokenStatus() {
  try {
    const result = await api<{ configured: boolean }>('/update/github-token')
    tokenConfigured.value = result.configured
  } catch {
    // 取れなくても画面は使える（未設定として扱う）
  }
}

async function loadDevBuilds() {
  devBuildsLoading.value = true
  devError.value = ''
  try {
    const result = await api<DevBuildsResponse>('/update/dev-builds')
    devBuilds.value = result
    tokenConfigured.value = result.token_configured
    if (result.error) devError.value = result.error
  } catch (e) {
    devError.value = e instanceof Error ? e.message : String(e)
  } finally {
    devBuildsLoading.value = false
  }
}

async function saveGithubToken() {
  devError.value = ''
  devMessage.value = ''
  try {
    await api('/update/github-token', {
      method: 'POST',
      body: JSON.stringify({ token: githubToken.value }),
    })
    githubToken.value = ''
    devMessage.value = 'トークンを保存しました。'
    await loadDevBuilds()
  } catch (e) {
    devError.value = e instanceof Error ? e.message : String(e)
  }
}

function formatSize(bytes: number | null) {
  if (!bytes) return '—'
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}

// GitHub API は ISO8601 (UTC) を返すので JST に直して表示する
function formatJst(iso: string | null) {
  if (!iso) return '—'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return d.toLocaleString('ja-JP', {
    timeZone: 'Asia/Tokyo',
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
  })
}

async function applyDevBuild(build: DevBuild) {
  if (build.artifact_id == null) return
  const ok = window.confirm(
    `開発版 (${build.branch ?? '?'} / ${(build.sha ?? '').slice(0, 7)}) に更新します。\n` +
      'proxy本体、recisdb、セットアップツールを同じビルドの一式へ更新します。WindowsではクライアントDLLも更新します。サーバー再起動時に視聴中・録画中のセッションはすべて切断されます。よろしいですか？',
  )
  if (!ok) return

  devApplying.value = build.artifact_id
  devError.value = ''
  devMessage.value = ''
  devStatus.value = '開始しています…'
  try {
    await api('/update/dev-build', {
      method: 'POST',
      body: JSON.stringify({
        artifact_id: build.artifact_id,
        label: `${build.branch ?? 'dev'}@${(build.sha ?? '').slice(0, 7)}`,
      }),
    })
    // 進捗は /update/status に出る。再起動まで見届けて自動で再読込する。
    const deadline = Date.now() + 10 * 60 * 1000
    while (Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 1000))
      try {
        const status = await api<{ state: string; message?: string | null }>('/update/status')
        devStatus.value = status.message || status.state
        if (status.state === 'error') {
          devError.value = status.message || '更新に失敗しました。'
          devApplying.value = null
          return
        }
        if (status.state === 'restarting') break
      } catch {
        // 再起動で一時的に落ちるのは想定内
        break
      }
    }
    devStatus.value = '再起動を待っています…'
    if (await waitForServer()) {
      location.reload()
    } else {
      devError.value = 'サーバーの再起動を確認できませんでした。手動で確認してください。'
    }
  } catch (e) {
    devError.value = e instanceof Error ? e.message : String(e)
  } finally {
    devApplying.value = null
  }
}

const previewSetup = ref<PreviewSetupReport | null>(null)
const previewSetupBusy = ref(false)
const previewSetupError = ref('')

const encoderSourceLabel: Record<string, string> = {
  detected: 'インストール済みのものを検出',
  downloaded: '自動ダウンロード',
  homebrew: 'Homebrewでインストール',
}

async function runPreviewAutoSetup() {
  previewSetupBusy.value = true
  previewSetupError.value = ''
  previewSetup.value = null
  try {
    const response = await api<{ report: PreviewSetupReport }>('/preview-config/auto-setup', {
      method: 'POST',
    })
    previewSetup.value = response.report
    // 「ブラウザプレビュー」を表示中なら、有効化された結果を反映する。
    if (selected.value === '/preview-config') await load()
  } catch (cause) {
    previewSetupError.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    previewSetupBusy.value = false
  }
}

// --- B-CASカードリーダーの選択 --------------------------------------
// 未選択だと libaribb25 が見つかったリーダーへ順に接続を試すため、B-CAS以外の
// リーダー(EMV等)が挿さっているとリーダー起動が十数秒待たされ、しかも間違った
// 方が採用されうる。接続されているものから名指しで選べるようにする。
type CardReaderResponse = {
  readers: string[]
  selected: string
  selected_present: boolean
}

const cardReaders = ref<CardReaderResponse | null>(null)
const cardReaderChoice = ref('')
const cardReaderMessage = ref('')
const cardReaderError = ref('')
const cardReaderBusy = ref(false)

async function loadCardReaders() {
  cardReaderBusy.value = true
  try {
    const response = await api<CardReaderResponse>('/card-reader')
    cardReaders.value = response
    cardReaderChoice.value = response.selected
    cardReaderError.value = ''
  } catch (cause) {
    cardReaderError.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    cardReaderBusy.value = false
  }
}

async function saveCardReader() {
  cardReaderBusy.value = true
  try {
    await api('/card-reader', {
      method: 'POST',
      body: JSON.stringify({ name: cardReaderChoice.value }),
    })
    cardReaderMessage.value = cardReaderChoice.value
      ? '保存しました。次にチューナーを開いたときから使われます。'
      : '自動選択に戻しました。次にチューナーを開いたときから有効になります。'
    cardReaderError.value = ''
    await loadCardReaders()
  } catch (cause) {
    cardReaderError.value = cause instanceof Error ? cause.message : String(cause)
    cardReaderMessage.value = ''
  } finally {
    cardReaderBusy.value = false
  }
}

function updateNumber(key: string, event: Event) {
  config.value[key] = Number((event.target as HTMLInputElement).value)
}
onMounted(() => {
  void load()
  void loadService()
  void loadCardReaders()
  void loadTokenStatus()
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
    <div class="panel">
      <h3>開発版に更新</h3>
      <p class="hint">
        GitHub Actions がビルドした最新の開発版に更新します。リリース版より新しい代わりに、
        検証されていない変更が含まれます。recisdbとセットアップツールも同じビルドへ更新し、WindowsではクライアントDLLも含めます。
      </p>

      <p v-if="tokenConfigured" class="muted">GitHubトークン: 設定済み</p>
      <p v-if="!tokenConfigured" class="hint">
        <strong>GitHubトークンが必要です。</strong>
        アーティファクトのダウンロードは公開リポジトリでも認証が必要なため、リリース版の更新と違い
        トークン無しでは実行できません。fine-grained トークンなら Actions: read、 classic
        トークンなら repo スコープを付けてください。
      </p>
      <form @submit.prevent="saveGithubToken">
        <label class="field"
          ><span>GitHubトークン{{ tokenConfigured ? '（設定済み・再設定する場合のみ）' : '' }}</span
          ><input
            v-model="githubToken"
            type="password"
            autocomplete="off"
            placeholder="ghp_... / github_pat_..." /></label
        ><button class="button secondary" type="submit">保存</button>
      </form>

      <div class="actions">
        <button class="button secondary" :disabled="devBuildsLoading" @click="loadDevBuilds">
          {{ devBuildsLoading ? '取得中…' : '開発版の一覧を取得' }}
        </button>
      </div>

      <p v-if="devMessage" class="muted" v-text="devMessage" />
      <p v-if="devError" class="error" v-text="devError" />
      <p v-if="devStatus" class="muted" v-text="devStatus" />

      <p v-if="devBuilds && !devBuilds.supported" class="hint" v-text="devBuilds.reason" />

      <div v-if="devBuilds?.supported && devBuilds.builds.length" class="table-region">
        <table class="data-table">
          <thead>
            <tr>
              <th>ブランチ</th>
              <th>コミット</th>
              <th>日時</th>
              <th>サイズ</th>
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="build in devBuilds.builds" :key="build.run_id">
              <td data-label="ブランチ" v-text="build.branch ?? '—'" />
              <td data-label="コミット" v-text="(build.sha ?? '').slice(0, 7) || '—'" />
              <td data-label="日時" v-text="formatJst(build.created_at)" />
              <td data-label="サイズ" v-text="formatSize(build.size_in_bytes)" />
              <td data-label="操作">
                <button
                  class="button small"
                  :disabled="!build.installable || !tokenConfigured || devApplying !== null"
                  @click="applyDevBuild(build)"
                >
                  {{ build.installable ? 'この版に更新' : '対象なし' }}
                </button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>
    <div class="panel">
      <h3>ブラウザプレビュー</h3>
      <p class="muted">
        ブラウザで映像を確認するには、エンコーダー（ffmpeg）と前段処理（tsreadex）が必要です。
        ボタンを押すと、すでに入っていればそれを使い、無ければ自動で用意して有効にします。
        ダウンロードとビルドを行うため、環境によっては数分かかります。
      </p>
      <p v-if="previewSetupError" class="notice error" role="alert" v-text="previewSetupError" />
      <dl v-if="previewSetup" class="service-status">
        <dt>エンコーダー</dt>
        <dd>
          {{ previewSetup.encoder_path }}（{{
            encoderSourceLabel[previewSetup.encoder_source] ?? previewSetup.encoder_source
          }}）
        </dd>
        <dt>映像エンコード</dt>
        <dd v-text="previewSetup.video_encoder" />
        <dt>前段処理</dt>
        <dd v-text="previewSetup.preprocessor_path || '（なし）'" />
      </dl>
      <p v-if="previewSetup && previewSetup.warnings.length === 0" class="notice success">
        プレビューを有効にしました。
      </p>
      <p
        v-for="warning in previewSetup?.warnings ?? []"
        :key="warning"
        class="notice"
        v-text="warning"
      />
      <button
        class="button"
        type="button"
        :disabled="previewSetupBusy"
        @click="runPreviewAutoSetup"
      >
        {{
          previewSetupBusy ? '準備中…（数分かかることがあります）' : 'プレビューを使えるようにする'
        }}
      </button>
    </div>
    <div class="panel">
      <h3>B-CASカードリーダー</h3>
      <p class="muted">
        カードリーダーが複数つながっている場合は、B-CASカードを挿しているものを選んでください。
        「自動」のままだと見つかった順に接続を試すため、B-CAS以外のリーダー（銀行カード用など）が
        あると視聴開始が数十秒遅くなったり、間違ったリーダーが選ばれることがあります。
      </p>
      <p v-if="cardReaderError" class="notice error" role="alert" v-text="cardReaderError" />
      <p v-if="cardReaderMessage" class="notice success" v-text="cardReaderMessage" />
      <p v-if="cardReaders && !cardReaders.selected_present" class="notice error" role="alert">
        選択中の「{{ cardReaders.selected }}」は現在つながっていません。
      </p>
      <p v-if="cardReaders && cardReaders.readers.length === 0" class="muted">
        カードリーダーが見つかりません。接続とPC/SCサービスの状態を確認してください。
      </p>
      <label v-else class="field"
        ><span>使用するカードリーダー</span
        ><select v-model="cardReaderChoice" :disabled="cardReaderBusy">
          <option value="">自動（見つかった順に試す）</option>
          <option v-for="name in cardReaders?.readers ?? []" :key="name" :value="name">
            {{ name }}
          </option>
        </select></label
      >
      <div class="actions">
        <button
          class="button secondary"
          type="button"
          :disabled="cardReaderBusy"
          @click="loadCardReaders"
        >
          再検出
        </button>
        <button class="button" type="button" :disabled="cardReaderBusy" @click="saveCardReader">
          保存
        </button>
      </div>
      <p class="muted">変更は次にチューナーを開いたときから反映されます。</p>
    </div>
    <p v-if="message" class="notice success" v-text="message" />
    <p v-if="error" class="notice error" role="alert" v-text="error" />
    <form class="panel settings-form" @submit.prevent="save">
      <h3 v-text="label" />
      <template v-if="isEpg">
        <section class="panel epg-status" aria-labelledby="epg-status-title">
          <h4 id="epg-status-title">EPG状態 <span>{{ epgStatus?.active ? '取得中' : '待機中' }}</span></h4>
          <div v-if="epgReasonGroups.length" class="muted" role="status"><p>取得を延期・終了した理由</p><ul><li v-for="group in epgReasonGroups" :key="group.code"><span class="epg-reason-main">{{ epgReasonLabel(group.reasons[0]) }}</span><span class="epg-reason-networks"><span v-for="(reason, index) in group.reasons" :key="index">{{ epgReasonNetworkText(reason) }}</span><span v-if="group.count > group.reasons.length">ほか{{ group.count - group.reasons.length }}系統</span></span></li></ul></div>
          <dl v-if="epgStatus?.state && typeof epgStatus.state === 'object'" class="service-status"><dt>最終取得</dt><dd>{{ formatEpoch(epgStateValue('lastScanCompletedAt')) }}</dd><dt>番組情報</dt><dd>{{ formatEpoch(epgStateValue('coverageUntil')) }}</dd></dl>
        </section>
        <p class="hint">番組表を自動更新します。録画・視聴を優先し、設定変更は次回の判定から反映されます。</p>
        <label class="check"><input v-model="config.enabled" type="checkbox" /><span>EPG自動取得を有効にする</span></label>
        <fieldset>
          <legend>更新方針とプリセット</legend>
          <p class="muted">組み込み初期値 → 全体設定 → プリセット → 物理チューナー個別設定の順に上書きされます。プリセットで空欄の項目は全体設定の値が使われます。</p>
          <div class="epg-preset-cards">
            <article v-for="preset in epgPresets" :key="String(preset.id)" class="panel epg-preset-card" :class="{ 'is-selected': selectedEpgPreset === Number(preset.id) }">
              <label class="preset-choice">
                <input v-model="selectedEpgPreset" type="radio" name="epg-preset" :value="Number(preset.id)" />
                <span class="epg-preset-content">
                <span class="epg-preset-heading"><strong v-text="String(preset.name)" /><span v-if="preset.is_system" class="badge">おすすめ</span><span v-if="selectedEpgPreset === Number(preset.id)" class="badge">使用中</span></span>
                <span class="muted" v-text="String(preset.description ?? '')" />
                <span class="epg-preset-summary"><span>更新頻度: {{ presetValue(preset.target_refresh_secs, 'seconds') }}</span><span>維持日数: {{ presetValue(preset.target_future_coverage_hours, 'hours') }}</span><span>CPU延期上限: {{ presetValue(preset.cpu_soft_limit_percent, 'percent') }}</span><span>録画・視聴で中断: {{ presetValue(preset.preemptible, 'boolean') }}</span></span>
                </span>
              </label>
              <span class="actions epg-preset-actions">
                <button class="button secondary" type="button" :disabled="Boolean(preset.is_system)" @click="editEpgPreset(preset)">{{ preset.is_system ? 'システム設定（複製して編集）' : '編集' }}</button>
                <button class="button secondary" type="button" @click="duplicateEpgPreset(preset)">複製</button>
                <button v-if="!preset.is_system" class="button secondary" type="button" @click="deleteEpgPreset(preset)">削除</button>
              </span>
            </article>
            <article class="panel epg-preset-card" :class="{ 'is-selected': selectedEpgPreset === null }">
              <label class="preset-choice">
                <input v-model="selectedEpgPreset" type="radio" name="epg-preset" :value="null" />
                <span class="epg-preset-content"><span class="epg-preset-heading"><strong>カスタム</strong><span v-if="selectedEpgPreset === null" class="badge">使用中</span></span><span class="muted">詳細設定を自分で決める</span><span class="epg-preset-summary"><span>全項目を個別に設定</span></span></span>
              </label>
            </article>
          </div>
          <div class="actions"><button class="button secondary" type="button" @click="startCreateEpgPreset">新規作成</button><a class="button secondary" href="#bondrivers">チューナー個別設定を開く</a></div>
          <p class="muted">プリセットを物理チューナーへ割り当てる場合は、<a href="#bondrivers">BonDriver（チューナー個別設定）</a>から設定します。</p>
        </fieldset>
        <section class="epg-presets" aria-labelledby="epg-presets-title">
          <h4 id="epg-presets-title">プリセットを編集</h4>
          <div v-if="editingPreset" class="panel preset-editor">
            <h5>{{ creatingPreset ? '新しいプリセット' : `プリセット編集: ${String(editingPreset.name)}` }}</h5>
            <label class="field"><span>名前</span><input v-model="editingPreset.name" type="text" /></label>
            <label class="field"><span>説明</span><textarea :value="String(editingPreset.description ?? '')" rows="2" @input="updatePresetText('description', $event)" /></label>
            <fieldset v-for="group in presetGroups" :key="group.label"><legend>{{ group.label }}</legend><div class="epg-friendly-grid"><label v-for="key in group.keys" :key="key" class="field"><span>{{ presetLabels[key] }} <small v-if="editingPreset[key] === null">（全体設定を使用）</small></span><input v-if="typeof editingPreset[key] === 'boolean'" v-model="editingPreset[key]" type="checkbox" /><input v-else :value="editingPreset[key] ?? ''" :placeholder="editingPreset[key] === null ? presetPlaceholder(key) : undefined" type="number" min="0" @input="updatePresetNumber(key, $event)" /><button v-if="editingPreset[key] !== null && key !== 'enabled'" class="button secondary" type="button" @click="clearPresetValue(key)">全体設定に戻す</button></label></div></fieldset>
            <div class="actions"><button class="button" type="button" @click="saveEpgPreset">保存</button><button class="button secondary" type="button" @click="editingPreset = null; creatingPreset = false">キャンセル</button></div>
          </div>
        </section>
        <div class="epg-friendly-grid">
          <label class="field"><span>更新頻度</span><select v-model.number="config.target_refresh_secs"><option :value="3600">約1時間</option><option :value="21600">約6時間</option><option :value="43200">約12時間</option></select></label>
          <label class="field"><span>何日先まで維持</span><select v-model.number="config.target_future_coverage_hours"><option :value="72">3日</option><option :value="168">7日</option><option :value="336">14日</option></select></label>
        </div>
        <label class="check"><input v-model="config.reserve_tuners" type="checkbox" /><span>空いているチューナーを確保して取得</span></label>
        <label class="check"><input v-model="config.preemptible" type="checkbox" /><span>録画・視聴が始まったら取得を中断</span></label>
        <label class="check"><input v-model="config.prefer_local" type="checkbox" /><span>ローカルチューナーを優先</span></label>
        <details><summary>エキスパート設定</summary>
          <p class="muted">秒単位の値。推奨: 最小30秒 / 通常90秒 / 最大180秒。</p>
          <div class="epg-friendly-grid"><label class="field"><span>最小滞在(秒)</span><input v-model.number="config.min_dwell_secs" type="number" min="1" /></label><label class="field"><span>通常滞在(秒)</span><input v-model.number="config.normal_dwell_secs" type="number" min="1" /></label><label class="field"><span>最大滞在(秒)</span><input v-model.number="config.max_dwell_secs" type="number" min="1" /></label><label class="field"><span>CPU上限(%)</span><input v-model.number="config.cpu_hard_limit_percent" type="number" min="1" max="100" /></label></div>
        </details>
        <p class="muted">この設定では、録画と視聴を優先し、最大{{ config.max_concurrent_scans }}台の取得チューナーを使います。</p>
      </template>
      <template v-for="[key, value] in entries" v-else :key="key">
        <label v-if="typeof value === 'boolean'" class="check"
          ><input v-model="config[key]" type="checkbox" :disabled="protectedKeys.has(key)" /><span
            v-text="displayName(key)"
        /></label>
        <label v-else-if="key === 'level'" class="field"
          ><span v-text="displayName(key)" /><select v-model="config[key]">
            <option v-for="lvl in LOG_LEVELS" :key="lvl" :value="lvl" v-text="lvl" /></select
        ></label>
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
      <p v-if="selected === '/log-config'" class="muted">
        ログレベルは保存すると即座に反映されます（再起動不要）。保持日数を超えた古いログは保存時に削除されます。出力先ディレクトリは起動オプション
        <code>--log-dir</code> で指定します。RUST_LOG
        が設定されている間は、そちらが起動時のレベルとして優先されます。
      </p>
      <p v-else class="muted">
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
  overflow-wrap: anywhere;
}

.epg-status li,
.epg-reason-networks,
.epg-reason-networks span {
  overflow-wrap: anywhere;
}

.epg-status li {
  display: grid;
  gap: 3px;
}

.epg-reason-main {
  color: var(--text-color, inherit);
}

.epg-reason-networks {
  display: flex;
  flex-wrap: wrap;
  gap: 2px 8px;
  font-size: 0.9em;
}

.epg-reason-networks span:not(:last-child)::after {
  content: ',';
}

.epg-preset-cards {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(100%, 220px), 1fr));
  gap: 12px;
}

.epg-preset-card {
  margin: 0;
  padding: 12px;
}

.epg-preset-card.is-selected {
  border-color: var(--accent-color, currentColor);
}

.preset-choice {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  min-height: 44px;
  cursor: pointer;
}

.epg-preset-actions {
  margin-top: 10px;
  flex-wrap: wrap;
}

.preset-choice > input {
  flex: 0 0 auto;
  margin-top: 4px;
}

.epg-preset-content {
  display: grid;
  flex: 1;
  gap: 8px;
  min-width: 0;
}

.epg-preset-heading {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
}

.epg-preset-summary {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 4px 12px;
  font-size: 0.9rem;
}

.epg-preset-card p {
  min-height: 2.5em;
}

@media (max-width: 700px) {
  .service-status {
    grid-template-columns: 1fr;
    gap: 2px;
  }

  .service-status dd {
    margin-bottom: 6px;
  }

  .epg-preset-cards {
    grid-template-columns: 1fr;
  }

  .epg-preset-summary {
    grid-template-columns: 1fr;
  }
}
</style>

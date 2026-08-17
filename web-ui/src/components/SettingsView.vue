<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, type JsonRecord } from '../api'

const endpoints = [
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
      '実行ファイルを入れ替えてサーバーを再起動します。視聴中・録画中のセッションはすべて切断されます。よろしいですか？',
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
        検証されていない変更が含まれます。
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
      <template v-for="[key, value] in entries" :key="key">
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
}
</style>

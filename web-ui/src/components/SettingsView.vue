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
function updateNumber(key: string, event: Event) {
  config.value[key] = Number((event.target as HTMLInputElement).value)
}
onMounted(load)
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

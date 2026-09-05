<script setup lang="ts">
import { ref, watch } from 'vue'
import { api, type JsonRecord } from '../api'
import EntityLookup, { type LookupEntity } from './EntityLookup.vue'

const props = defineProps<{ tunerId: number; tunerName: string }>()
const emit = defineEmits<{ saved: [] }>()
const presets = ref<LookupEntity[]>([])
const mode = ref<'global' | 'preset' | 'override'>('global')
const presetId = ref<number | null>(null)
const settings = ref<JsonRecord>({})
const effective = ref<JsonRecord>({})
const source = ref<JsonRecord>({})
const loading = ref(false)
const message = ref('')
const error = ref('')
const override = ref({ enabled_override: true, auto_tuner_scan_enabled_override: null as boolean | null, target_refresh_secs_override: null as number | null, min_dwell_secs_override: null as number | null, normal_dwell_secs_override: null as number | null, max_dwell_secs_override: null as number | null, allow_remote_override: null as boolean | null, prefer_local_override: null as boolean | null, preemptible_override: null as boolean | null })

async function load() {
  loading.value = true
  try {
    const [result, presetResult] = await Promise.all([api<JsonRecord>(`/tuners/${props.tunerId}/epg-settings`), api<JsonRecord>('/epg-presets')])
    settings.value = (result.settings as JsonRecord) ?? {}
    const value = result.effective as JsonRecord
    effective.value = (value?.effective as JsonRecord) ?? {}
    source.value = (value?.source as JsonRecord) ?? {}
    presets.value = Array.isArray(presetResult.presets) ? (presetResult.presets as JsonRecord[]).map((p) => ({ id: Number(p.id), name: String(p.name), description: String(p.description ?? '') })) : []
    presetId.value = typeof settings.value.preset_id === 'number' ? Number(settings.value.preset_id) : null
    mode.value = presetId.value == null ? 'global' : 'preset'
    if (settings.value.enabled_override != null || settings.value.auto_tuner_scan_enabled_override != null || settings.value.target_refresh_secs_override != null) {
      mode.value = 'override'
      Object.assign(override.value, settings.value)
    }
    error.value = ''
  } catch (cause) { error.value = cause instanceof Error ? cause.message : String(cause) } finally { loading.value = false }
}
async function save() {
  const payload: JsonRecord = mode.value === 'global' ? { physical_tuner_id: props.tunerId } : mode.value === 'preset' ? { physical_tuner_id: props.tunerId, preset_id: presetId.value } : { physical_tuner_id: props.tunerId, ...override.value }
  try {
    const result = await api<JsonRecord>(`/tuners/${props.tunerId}/epg-settings`, { method: 'PUT', body: JSON.stringify(payload) })
    const value = result.effective as JsonRecord
    effective.value = (value?.effective as JsonRecord) ?? effective.value
    source.value = (value?.source as JsonRecord) ?? source.value
    settings.value = (result.settings as JsonRecord) ?? settings.value
    message.value = 'このチューナーのEPG設定を保存しました。次回の判定から適用されます。'
    emit('saved')
  } catch (cause) { error.value = cause instanceof Error ? cause.message : String(cause) }
}
function resetRecommended() { Object.assign(override.value, { enabled_override: true, auto_tuner_scan_enabled_override: null, target_refresh_secs_override: null, min_dwell_secs_override: null, normal_dwell_secs_override: null, max_dwell_secs_override: null, allow_remote_override: null, prefer_local_override: null, preemptible_override: null }) }
function sourceLabel(key: string): string { const value = source.value[key]; return value === 'tunerOverride' ? 'このチューナー' : value === 'preset' ? 'プリセット' : '全体設定' }
watch(() => props.tunerId, load, { immediate: true })
</script>

<template>
  <section class="panel physical-epg-settings" aria-labelledby="physical-epg-title">
    <h3 id="physical-epg-title">{{ tunerName }} / EPG自動取得</h3>
    <p v-if="loading" class="muted">読み込み中…</p>
    <template v-else>
      <fieldset>
        <legend>設定方法</legend>
        <label class="preset-choice"><input v-model="mode" type="radio" value="global" />全体設定を使用</label>
        <label class="preset-choice"><input v-model="mode" type="radio" value="preset" />プリセットを使用</label>
        <label class="preset-choice"><input v-model="mode" type="radio" value="override" />このチューナーだけ個別設定</label>
      </fieldset>
      <EntityLookup v-if="mode === 'preset'" v-model="presetId" :entities="presets" label="EPGプリセット" placeholder="名前・説明で検索" />
      <div v-if="mode === 'override'" class="epg-override-fields">
        <label class="check"><input v-model="override.enabled_override" type="checkbox" />このチューナーでEPGを取得</label>
        <label class="check"><input v-model="override.auto_tuner_scan_enabled_override" type="checkbox" />チューナーを自動起動してEPGを収集する</label>
        <p class="hint">OFF にすると、番組表のためだけにこのチューナーを起動しません。視聴・録画中は番組表も取り込みます。</p>
        <details open><summary>基本設定</summary><label class="field"><span>更新頻度</span><select v-model.number="override.target_refresh_secs_override"><option :value="null">全体設定を使用</option><option :value="3600">約1時間</option><option :value="21600">約6時間</option><option :value="43200">約12時間</option></select></label></details>
        <details><summary>詳細設定</summary><p class="muted">未入力の項目は全体設定から引き継ぎます。推奨: 滞在 30 / 90 / 180 秒。</p><div class="epg-friendly-grid"><label class="field"><span>最小滞在（秒）</span><input v-model.number="override.min_dwell_secs_override" type="number" min="1" /></label><label class="field"><span>通常滞在（秒）</span><input v-model.number="override.normal_dwell_secs_override" type="number" min="1" /></label><label class="field"><span>最大滞在（秒）</span><input v-model.number="override.max_dwell_secs_override" type="number" min="1" /></label></div></details>
        <button class="button secondary" type="button" @click="resetRecommended">推奨値に戻す</button>
      </div>
      <aside class="epg-effective" aria-live="polite"><strong>現在適用中</strong><p>更新頻度: 約{{ Math.round(Number(effective.target_refresh_secs ?? 0) / 3600) }}時間（{{ sourceLabel('target_refresh_secs') }}）</p><p>EPG取得: {{ effective.enabled ? '有効' : '停止' }}（{{ sourceLabel('enabled') }}）</p><p>録画・視聴を優先: {{ effective.preemptible ? 'はい' : 'いいえ' }}（{{ sourceLabel('preemptible') }}）</p></aside>
      <div class="actions"><button class="button" type="button" @click="save">保存</button></div>
      <p v-if="message" class="notice success" v-text="message" /><p v-if="error" class="notice error" role="alert" v-text="error" />
    </template>
  </section>
</template>

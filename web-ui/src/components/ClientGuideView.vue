<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, downloadApi, unwrapArray, type JsonRecord } from '../api'
import DataTable from './DataTable.vue'
import { useColumnVisibility, type ColumnDef } from '../columns'
import ColumnPicker from './ColumnPicker.vue'

const targetColumns: ColumnDef[] = [
  { key: 'name', label: '名前' },
  { key: 'type', label: '種別' },
  { key: 'enabled_channels', label: '有効チャンネル' },
  { key: 'note', label: '備考' },
]
const {
  visibleKeys: targetVisibleKeys,
  isVisible: targetIsVisible,
  setColumn: targetSetColumn,
  resetColumns: targetResetColumns,
} = useColumnVisibility('columns:client-guide-targets', () => targetColumns)

const targets = ref<JsonRecord[]>([])
const selected = ref('')
const proxyPort = ref(40070)
const info = ref<JsonRecord>({})
const error = ref('')
const message = ref('')
const selectedTarget = computed(() =>
  targets.value.find((target) => String(target.name ?? target.tuner ?? '') === selected.value),
)
const ini = computed(
  () =>
    `[Server]\nAddress = ${location.hostname || '127.0.0.1'}:${proxyPort.value}\nTuner = ${selected.value || '(STEP 1で選択)'}\n`,
)

async function load() {
  try {
    const result = await api<JsonRecord>('/client-view/targets')
    targets.value = unwrapArray(result, ['targets', 'data'])
    proxyPort.value = Number(result.proxy_port ?? 40070)
    if (
      !targets.value.some((target) => String(target.name ?? target.tuner ?? '') === selected.value)
    ) {
      const usable =
        targets.value.find((target) => Number(target.enabled_channels ?? 0) > 0) ?? targets.value[0]
      selected.value = usable ? String(usable.name ?? usable.tuner ?? '') : ''
    }
    await loadView()
    error.value = ''
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}
function selectTarget(target: JsonRecord) {
  selected.value = String(target.name ?? target.tuner ?? '')
  loadView()
}
async function loadView() {
  if (!selected.value) {
    info.value = {}
    return
  }
  try {
    info.value = await api<JsonRecord>(`/client-view?tuner=${encodeURIComponent(selected.value)}`)
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}
async function copyIni() {
  try {
    await navigator.clipboard.writeText(ini.value)
    message.value = 'INI設定をコピーしました。'
  } catch {
    message.value = 'コピーできませんでした。表示内容を選択してコピーしてください。'
  }
}
async function file(kind: string, fallback: string) {
  if (!selected.value) {
    error.value = '先に接続先チューナーを選択してください。'
    return
  }
  try {
    await downloadApi(
      `/client-view/files/${kind}?tuner=${encodeURIComponent(selected.value)}`,
      fallback,
    )
    message.value = `${fallback} をダウンロードしました。`
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}
const spaces = computed(() =>
  Array.isArray(info.value.spaces) ? (info.value.spaces as JsonRecord[]) : [],
)
function rowsFor(space: JsonRecord): JsonRecord[] {
  return Array.isArray(space.channels) ? (space.channels as JsonRecord[]) : []
}
function physical(channel: JsonRecord) {
  const sources = Array.isArray(channel.physical) ? channel.physical : []
  return (
    sources
      .map((value) => {
        const row = value as JsonRecord
        return `${String(row.driver ?? '—')} (Space ${String(row.space ?? '—')} / Ch ${String(row.channel ?? '—')})`
      })
      .join('\n') || '—'
  )
}
const previewOpen = ref(false)
function openPreview() {
  if (!selected.value) {
    error.value = '先に接続先チューナーを選択してください。'
    return
  }
  previewOpen.value = true
}
function onKeydown(event: KeyboardEvent) {
  if (event.key === 'Escape') previewOpen.value = false
}
onMounted(() => {
  window.addEventListener('keydown', onKeydown)
  void load()
})
onUnmounted(() => window.removeEventListener('keydown', onKeydown))
</script>
<template>
  <section class="view">
    <div class="view-heading">
      <div>
        <h2>クライアント設定ガイド</h2>
        <p>TVTest / EDCBで利用する接続先と設定ファイルを作成します。</p>
      </div>
      <button class="button secondary" @click="load">更新</button>
    </div>
    <p v-if="message" class="notice success" v-text="message" />
    <p v-if="error" class="notice error" role="alert" v-text="error" />
    <h3>STEP 1. 接続先チューナーを選択</h3>
    <ColumnPicker
      :columns="targetColumns"
      :visible-keys="targetVisibleKeys"
      @set="targetSetColumn"
      @reset="targetResetColumns"
    />
    <div class="table-region">
      <table v-if="targets.length" class="data-table">
        <thead>
          <tr>
            <th />
            <th v-if="targetIsVisible('name')">名前</th>
            <th v-if="targetIsVisible('type')">種別</th>
            <th v-if="targetIsVisible('enabled_channels')">有効チャンネル</th>
            <th v-if="targetIsVisible('note')">備考</th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="target in targets"
            :key="String(target.name ?? target.tuner)"
            @click="selectTarget(target)"
          >
            <td>
              <input
                v-model="selected"
                type="radio"
                name="tuner"
                :value="String(target.name ?? target.tuner ?? '')"
                @change="loadView"
              />
            </td>
            <td v-if="targetIsVisible('name')" data-label="名前">
              <code v-text="String(target.name ?? target.tuner ?? '—')" />
            </td>
            <td
              v-if="targetIsVisible('type')"
              data-label="種別"
              v-text="String(target.type ?? 'チューナー単体')"
            />
            <td
              v-if="targetIsVisible('enabled_channels')"
              data-label="有効チャンネル"
              v-text="String(target.enabled_channels ?? 0)"
            />
            <td
              v-if="targetIsVisible('note')"
              data-label="備考"
              v-text="String(target.display_name ?? target.note ?? '—')"
            />
          </tr>
        </tbody>
      </table>
      <p v-else class="empty-state">
        BonDriverが未登録です。先にBonDriverタブで登録・スキャンしてください。
      </p>
    </div>
    <h3>STEP 2. BonDriver_NetworkProxy.ini を作成</h3>
    <pre class="code-block" v-text="ini" />
    <button class="button secondary" @click="copyIni">INI設定をコピー</button>
    <h3>STEP 3. チャンネル設定をダウンロード</h3>
    <div class="actions">
      <button class="button secondary" @click="file('tvtest-ch2', 'BonDriver_NetworkProxy.ch2')">
        TVTest .ch2</button
      ><button class="button secondary" @click="file('chset4', 'ChSet4.txt')">EDCB ChSet4</button
      ><button class="button secondary" @click="file('chset5', 'ChSet5.txt')">EDCB ChSet5</button
      ><button class="button" @click="file('bundle', 'recisdb-proxy-client-config.zip')">
        まとめてダウンロード
      </button>
    </div>
    <h3>STEP 4. クライアントに表示されるチャンネル</h3>
    <p class="muted">選択中のチューナーでクライアントに列挙されるチャンネルを確認できます。</p>
    <button class="button secondary" @click="openPreview">チャンネルプレビューを表示</button>
    <div v-if="previewOpen" class="dialog-backdrop" @click.self="previewOpen = false">
      <section
        class="dialog preview-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="channel-preview-title"
      >
        <div class="view-heading">
          <div>
            <h2 id="channel-preview-title">チャンネルプレビュー</h2>
            <p
              v-if="selectedTarget"
              class="muted"
              v-text="`接続先: ${String(selectedTarget.name ?? selectedTarget.tuner)}`"
            />
          </div>
          <button class="button secondary" @click="previewOpen = false">閉じる</button>
        </div>
        <template v-for="space in spaces" :key="String(space.index)">
          <h4
            v-text="`チューニング空間 ${String(space.index ?? '')}: ${String(space.name ?? '')}`"
          />
          <DataTable
            :rows="
              rowsFor(space).map((channel) => ({
                index: channel.index,
                name: channel.name,
                physical: physical(channel),
              }))
            "
            :columns="['index', 'name', 'physical']"
            storage-key="columns:client-guide-preview"
            empty="有効なチャンネルはありません"
          />
        </template>
        <p v-if="!spaces.length" class="empty-state">
          このチューナーには有効なチャンネルがありません。
        </p>
      </section>
    </div>
  </section>
</template>

<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { api, unwrapArray, type JsonRecord } from '../api'
import { useColumnVisibility, type ColumnDef } from '../columns'
import ColumnPicker from './ColumnPicker.vue'

const profileColumns: ColumnDef[] = [
  { key: 'is_enabled', label: '有効' },
  { key: 'name', label: '名前' },
  { key: 'purpose', label: '用途' },
  { key: 'codec', label: 'コーデック' },
  { key: 'container', label: 'コンテナ' },
  { key: 'target_bitrate', label: 'ビットレート' },
  { key: 'extra_args', label: '追加引数' },
]
const {
  visibleKeys: profileVisibleKeys,
  isVisible: profileIsVisible,
  setColumn: profileSetColumn,
  resetColumns: profileResetColumns,
} = useColumnVisibility('columns:encode-profiles', () => profileColumns)

const profiles = ref<JsonRecord[]>([])
const error = ref('')
const message = ref('')
const editingId = ref<number | null>(null)
const form = reactive({
  name: '',
  purpose: 'preview',
  codec: 'h264',
  container: 'mpegts',
  target_bitrate: '' as number | '',
  extra_args: '',
  is_enabled: true,
})

async function load() {
  try {
    profiles.value = unwrapArray(await api<unknown>('/encode-profiles'), ['profiles', 'data'])
    error.value = ''
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}

function reset() {
  editingId.value = null
  Object.assign(form, {
    name: '',
    purpose: 'preview',
    codec: 'h264',
    container: 'mpegts',
    target_bitrate: '',
    extra_args: '',
    is_enabled: true,
  })
}

function edit(profile: JsonRecord) {
  editingId.value = Number(profile.id)
  form.name = String(profile.name ?? '')
  form.purpose = String(profile.purpose ?? 'preview')
  form.codec = String(profile.codec ?? 'h264')
  form.container = String(profile.container ?? 'mpegts')
  form.target_bitrate = profile.target_bitrate == null ? '' : Number(profile.target_bitrate)
  form.extra_args = String(profile.extra_args ?? '')
  form.is_enabled = Boolean(profile.is_enabled)
  message.value = ''
}

function payload() {
  return {
    name: form.name.trim(),
    purpose: form.purpose.trim(),
    codec: form.codec.trim(),
    container: form.container.trim() || 'mpegts',
    target_bitrate: form.target_bitrate === '' ? null : Number(form.target_bitrate),
    extra_args: form.extra_args.trim() || null,
    is_enabled: form.is_enabled,
  }
}

async function save() {
  try {
    const target =
      editingId.value == null ? '/encode-profiles' : `/encode-profiles/${editingId.value}`
    await api(target, { method: 'POST', body: JSON.stringify(payload()) })
    message.value =
      editingId.value == null ? 'プロファイルを追加しました。' : 'プロファイルを更新しました。'
    reset()
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}

async function toggle(profile: JsonRecord, event: Event) {
  const checked = (event.target as HTMLInputElement).checked
  try {
    await api(`/encode-profiles/${profile.id}`, {
      method: 'POST',
      body: JSON.stringify({ is_enabled: checked }),
    })
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}

async function remove(profile: JsonRecord) {
  if (!confirm(`「${String(profile.name)}」を削除しますか？`)) return
  try {
    await api(`/encode-profiles/${profile.id}`, { method: 'DELETE' })
    if (editingId.value === Number(profile.id)) reset()
    message.value = 'プロファイルを削除しました。'
    await load()
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  }
}

onMounted(load)
</script>

<template>
  <section class="view">
    <div class="view-heading">
      <div>
        <h2>エンコードプロファイル</h2>
        <p>ブラウザプレビュー／BNDP配信に使うコーデック・ビットレート・追加引数を管理します。</p>
      </div>
      <div class="actions">
        <button class="button secondary" @click="reset">新規追加</button>
        <button class="button secondary" @click="load">更新</button>
      </div>
    </div>
    <p v-if="message" class="notice success" v-text="message" />
    <p v-if="error" class="notice error" role="alert" v-text="error" />
    <div class="split">
      <div class="table-region">
        <ColumnPicker
          :columns="profileColumns"
          :visible-keys="profileVisibleKeys"
          @set="profileSetColumn"
          @reset="profileResetColumns"
        />
        <table v-if="profiles.length" class="data-table">
          <thead>
            <tr>
              <th v-if="profileIsVisible('is_enabled')">有効</th>
              <th v-if="profileIsVisible('name')">名前</th>
              <th v-if="profileIsVisible('purpose')">用途</th>
              <th v-if="profileIsVisible('codec')">コーデック</th>
              <th v-if="profileIsVisible('container')">コンテナ</th>
              <th v-if="profileIsVisible('target_bitrate')">ビットレート</th>
              <th v-if="profileIsVisible('extra_args')">追加引数</th>
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="profile in profiles" :key="String(profile.id)">
              <td v-if="profileIsVisible('is_enabled')" data-label="有効">
                <input
                  :checked="Boolean(profile.is_enabled)"
                  type="checkbox"
                  @change="toggle(profile, $event)"
                />
              </td>
              <td
                v-if="profileIsVisible('name')"
                data-label="名前"
                v-text="String(profile.name ?? '—')"
              />
              <td
                v-if="profileIsVisible('purpose')"
                data-label="用途"
                v-text="String(profile.purpose ?? '—')"
              />
              <td
                v-if="profileIsVisible('codec')"
                data-label="コーデック"
                v-text="String(profile.codec ?? '—')"
              />
              <td
                v-if="profileIsVisible('container')"
                data-label="コンテナ"
                v-text="String(profile.container ?? 'mpegts')"
              />
              <td
                v-if="profileIsVisible('target_bitrate')"
                data-label="ビットレート"
                v-text="
                  profile.target_bitrate == null
                    ? '—'
                    : `${Number(profile.target_bitrate) / 1000} kbps`
                "
              />
              <td
                v-if="profileIsVisible('extra_args')"
                data-label="追加引数"
                v-text="String(profile.extra_args ?? '—')"
              />
              <td data-label="操作">
                <div class="actions">
                  <button class="button small secondary" @click="edit(profile)">編集</button
                  ><button class="button small danger" @click="remove(profile)">削除</button>
                </div>
              </td>
            </tr>
          </tbody>
        </table>
        <p v-else class="empty-state">エンコードプロファイルがありません</p>
      </div>
      <form class="panel" @submit.prevent="save">
        <h3 v-text="editingId == null ? 'プロファイル追加' : 'プロファイル編集'" />
        <label class="field"
          ><span>名前</span><input v-model="form.name" required placeholder="例: preview-h264"
        /></label>
        <label class="field"
          ><span>用途</span
          ><select v-model="form.purpose">
            <option value="preview">ブラウザプレビュー</option>
            <option value="bndp">BNDP</option>
            <option value="record">録画</option>
          </select></label
        >
        <label class="field"
          ><span>コーデック</span
          ><select v-model="form.codec">
            <option value="h264">H.264</option>
            <option value="hevc">HEVC / H.265</option>
            <option value="mpeg2">MPEG-2</option>
            <option value="copy">コピー</option>
          </select></label
        >
        <label class="field"
          ><span>コンテナ</span><input v-model="form.container" placeholder="mpegts"
        /></label>
        <label class="field"
          ><span>目標ビットレート (bps)</span
          ><input v-model.number="form.target_bitrate" type="number" min="0" placeholder="未指定"
        /></label>
        <label class="field"
          ><span>追加引数</span
          ><input v-model="form.extra_args" placeholder="エンコーダーへ渡す引数"
        /></label>
        <label class="check"><input v-model="form.is_enabled" type="checkbox" />有効にする</label>
        <div class="actions">
          <button class="button" type="submit">保存</button
          ><button class="button secondary" type="button" @click="reset">取消</button>
        </div>
      </form>
    </div>
  </section>
</template>

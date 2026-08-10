<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { api, unwrapArray, type JsonRecord } from '../api'
import { useColumnVisibility, type ColumnDef } from '../columns'
import ColumnPicker from './ColumnPicker.vue'
const rankingColumns: ColumnDef[] = [
  { key: 'driver', label: 'BonDriver' },
  { key: 'quality_score', label: '品質スコア' },
  { key: 'recent_drop_rate', label: '直近Drop率' },
  { key: 'total_sessions', label: '総セッション' },
]
const {
  visibleKeys: rankingVisibleKeys,
  isVisible: rankingIsVisible,
  setColumn: rankingSetColumn,
  resetColumns: rankingResetColumns,
} = useColumnVisibility('columns:bondriver-ranking', () => rankingColumns)
const listColumns: ColumnDef[] = [
  { key: 'name', label: '名前' },
  { key: 'group', label: 'グループ' },
  { key: 'max_instances', label: '最大数' },
  { key: 'stream_format', label: '形式' },
  { key: 'state', label: '状態' },
]
const {
  visibleKeys: listVisibleKeys,
  isVisible: listIsVisible,
  setColumn: listSetColumn,
  resetColumns: listResetColumns,
} = useColumnVisibility('columns:bondriver-list', () => listColumns)
const rows = ref<JsonRecord[]>([])
const ranking = ref<JsonRecord[]>([])
const error = ref('')
const editing = ref<number | null>(null)
const form = reactive({
  dll_path: '',
  driver_name: '',
  group_name: '',
  max_instances: 1,
  stream_format: 'ts',
  disable_b25: false,
  auto_scan_enabled: false,
  scan_interval_hours: 24,
  scan_priority: 0,
  passive_scan_enabled: false,
})
async function load() {
  try {
    const [drivers, rankingResult] = await Promise.all([
      api<unknown>('/bondrivers'),
      api<unknown>('/bondrivers/ranking'),
    ])
    rows.value = unwrapArray(drivers, ['bondrivers'])
    ranking.value = unwrapArray(rankingResult, ['items', 'ranking', 'data'])
    error.value = ''
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}
function reset() {
  editing.value = null
  Object.assign(form, {
    dll_path: '',
    driver_name: '',
    group_name: '',
    max_instances: 1,
    stream_format: 'ts',
    disable_b25: false,
    auto_scan_enabled: false,
    scan_interval_hours: 24,
    scan_priority: 0,
    passive_scan_enabled: false,
  })
}
function edit(row: JsonRecord) {
  editing.value = Number(row.id)
  for (const key of Object.keys(form))
    if (row[key] != null) (form as Record<string, unknown>)[key] = row[key]
}
async function save() {
  try {
    await api(editing.value ? `/bondriver/${editing.value}` : '/bondriver', {
      method: 'POST',
      body: JSON.stringify(form),
    })
    reset()
    await load()
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}
async function remove(id: unknown) {
  if (!confirm('BonDriverを削除しますか？')) return
  await api(`/bondriver/${id}`, { method: 'DELETE' })
  await load()
}
async function scan(id: unknown) {
  await api(`/bondriver/${id}/scan`, { method: 'POST' })
  alert('スキャンを予約しました')
}
function rankedDriver(item: JsonRecord): JsonRecord {
  const value = item.driver
  return value && typeof value === 'object' && !Array.isArray(value) ? (value as JsonRecord) : {}
}
onMounted(load)
</script>
<template>
  <section class="view">
    <div class="view-heading">
      <div>
        <h2>BonDriver</h2>
        <p>チューナードライバーの登録・編集・スキャン</p>
      </div>
      <button class="button secondary" @click="reset">新規登録</button>
    </div>
    <p v-if="error" class="notice error" v-text="error" />
    <section class="quality-panel">
      <h3>品質ランキング</h3>
      <ColumnPicker
        :columns="rankingColumns"
        :visible-keys="rankingVisibleKeys"
        @set="rankingSetColumn"
        @reset="rankingResetColumns"
      />
      <div class="table-region">
        <table v-if="ranking.length" class="data-table compact">
          <thead>
            <tr>
              <th v-if="rankingIsVisible('driver')">BonDriver</th>
              <th v-if="rankingIsVisible('quality_score')">品質スコア</th>
              <th v-if="rankingIsVisible('recent_drop_rate')">直近Drop率</th>
              <th v-if="rankingIsVisible('total_sessions')">総セッション</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="item in ranking" :key="String(rankedDriver(item).id ?? item.id)">
              <td
                v-if="rankingIsVisible('driver')"
                data-label="BonDriver"
                v-text="
                  String(rankedDriver(item).driver_name ?? rankedDriver(item).dll_path ?? '—')
                "
              />
              <td
                v-if="rankingIsVisible('quality_score')"
                data-label="品質スコア"
                v-text="String(item.quality_score ?? '—')"
              />
              <td
                v-if="rankingIsVisible('recent_drop_rate')"
                data-label="直近Drop率"
                v-text="item.recent_drop_rate == null ? '—' : `${String(item.recent_drop_rate)}%`"
              />
              <td
                v-if="rankingIsVisible('total_sessions')"
                data-label="総セッション"
                v-text="String(item.total_sessions ?? 0)"
              />
            </tr>
          </tbody>
        </table>
        <p v-else class="empty-state">品質データはまだありません</p>
      </div>
    </section>
    <div class="split">
      <div class="table-region">
        <ColumnPicker
          :columns="listColumns"
          :visible-keys="listVisibleKeys"
          @set="listSetColumn"
          @reset="listResetColumns"
        />
        <table class="data-table compact">
          <thead>
            <tr>
              <th v-if="listIsVisible('name')">名前</th>
              <th v-if="listIsVisible('group')">グループ</th>
              <th v-if="listIsVisible('max_instances')">最大数</th>
              <th v-if="listIsVisible('stream_format')">形式</th>
              <th v-if="listIsVisible('state')">状態</th>
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="row in rows" :key="String(row.id)">
              <td
                v-if="listIsVisible('name')"
                data-label="名前"
                v-text="String(row.driver_name ?? row.dll_path ?? '—')"
              />
              <td
                v-if="listIsVisible('group')"
                data-label="グループ"
                v-text="String(row.group_name ?? '—')"
              />
              <td
                v-if="listIsVisible('max_instances')"
                data-label="最大数"
                v-text="String(row.max_instances ?? 1)"
              />
              <td v-if="listIsVisible('stream_format')" data-label="形式">
                <span v-if="row.stream_format === 'mmttlv'" class="badge">4K MMT/TLV</span>
                <span v-else>TS</span>
                <span v-if="row.disable_b25" class="badge muted">B25無効</span>
              </td>
              <!-- スキャンは視聴と同じくチューナー枠を1つ占有する。
                   占有されている理由が分からないと、視聴できない原因を
                   利用者が追えないため状態として出す。 -->
              <td v-if="listIsVisible('state')" data-label="状態">
                <span v-if="row.is_scanning" class="badge badge-scanning">スキャン中</span>
                <span v-else>—</span>
              </td>
              <td data-label="操作">
                <div class="actions">
                  <button class="button small secondary" @click="edit(row)">編集</button
                  ><button class="button small secondary" @click="scan(row.id)">スキャン</button
                  ><button class="button small danger" @click="remove(row.id)">削除</button>
                </div>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <form class="panel" @submit.prevent="save">
        <h3 v-text="editing ? 'BonDriver編集' : 'BonDriver登録'" />
        <label class="field"><span>DLLパス</span><input v-model="form.dll_path" required /></label
        ><label class="field"><span>表示名</span><input v-model="form.driver_name" /></label
        ><label class="field"><span>グループ名</span><input v-model="form.group_name" /></label
        ><label class="field"
          ><span>最大インスタンス</span
          ><input v-model.number="form.max_instances" type="number" min="1" /></label
        ><label class="field"
          ><span>ストリーム形式</span
          ><select v-model="form.stream_format">
            <option value="ts">MPEG-2 TS（通常）</option>
            <option value="mmttlv">MMT/TLV（BS4K・要変換器）</option>
          </select></label
        >
        <p class="hint">
          MMT/TLV を選ぶと、受信した生ストリームを外部変換器（dantto4k）で TS
          に変換してから配信・スキャンします。変換器のパスと ACAS の設定は設定ファイルの [mmttlv]
          セクションで行います。
        </p>
        <label class="check"
          ><input
            v-model="form.disable_b25"
            type="checkbox"
          />B25（スクランブル解除）を使わない</label
        >
        <p class="hint">
          既に復号済みのソース向け。MMT/TLV と 4K のチャンネルは指定がなくても
          自動で無効になります。
        </p>
        <label class="check"
          ><input v-model="form.auto_scan_enabled" type="checkbox" />自動スキャン</label
        ><label class="field"
          ><span>スキャン間隔（時間）</span
          ><input
            v-model.number="form.scan_interval_hours"
            type="number"
            min="1"
            max="720" /></label
        ><label class="field"
          ><span>スキャン優先度</span
          ><input v-model.number="form.scan_priority" type="number" min="0" max="100" /></label
        ><label class="check"
          ><input v-model="form.passive_scan_enabled" type="checkbox" />パッシブスキャン</label
        ><button class="button" type="submit">保存</button>
      </form>
    </div>
  </section>
</template>

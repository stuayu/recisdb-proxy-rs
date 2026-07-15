<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { api, unwrapArray, type JsonRecord } from '../api'
const alerts = ref<JsonRecord[]>([]),
  rules = ref<JsonRecord[]>([]),
  error = ref('')
const form = reactive({
  name: '',
  metric: 'drop_rate',
  condition: 'greater_than',
  threshold: 1,
  severity: 'warning',
  is_enabled: true,
  webhook_url: '',
  webhook_format: 'json',
})
async function load() {
  try {
    alerts.value = unwrapArray(await api('/alerts'), ['alerts'])
    rules.value = unwrapArray(await api('/alert-rules'), ['rules'])
    error.value = ''
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}
async function create() {
  await api('/alert-rules', { method: 'POST', body: JSON.stringify(form) })
  form.name = ''
  await load()
}
async function remove(id: unknown) {
  if (confirm('ルールを削除しますか？')) {
    await api(`/alert-rules/${id}`, { method: 'DELETE' })
    await load()
  }
}
async function acknowledge(id: unknown) {
  await api(`/alerts/${id}/acknowledge`, { method: 'POST' })
  await load()
}
onMounted(load)
</script>
<template>
  <section class="view">
    <div class="view-heading">
      <div>
        <h2>アラート</h2>
        <p>発生中アラートと通知ルール</p>
      </div>
      <button class="button secondary" @click="load">更新</button>
    </div>
    <p v-if="error" class="notice error" v-text="error"></p>
    <h3>発生中</h3>
    <div class="cards">
      <article v-for="item in alerts" :key="String(item.id)" class="alert-card">
        <div>
          <strong v-text="String(item.message ?? item.metric ?? 'アラート')"></strong>
          <p v-text="String(item.severity ?? 'warning')"></p>
        </div>
        <button class="button small" @click="acknowledge(item.id)">確認済み</button>
      </article>
      <p v-if="!alerts.length" class="empty-state">発生中のアラートはありません</p>
    </div>
    <div class="split">
      <div>
        <h3>ルール</h3>
        <div class="cards">
          <article v-for="rule in rules" :key="String(rule.id)" class="alert-card">
            <div>
              <strong v-text="String(rule.name ?? '—')"></strong>
              <p
                v-text="
                  `${String(rule.metric ?? '')} ${String(rule.condition ?? '')} ${String(rule.threshold ?? '')}`
                "
              ></p>
            </div>
            <button class="button small danger" @click="remove(rule.id)">削除</button>
          </article>
        </div>
      </div>
      <form class="panel" @submit.prevent="create">
        <h3>ルール追加</h3>
        <label class="field"><span>名前</span><input v-model="form.name" required /></label
        ><label class="field"
          ><span>メトリクス</span
          ><select v-model="form.metric">
            <option value="drop_rate">Drop率</option>
            <option value="bitrate">ビットレート</option>
            <option value="signal_level">信号レベル</option>
          </select></label
        ><label class="field"
          ><span>条件</span
          ><select v-model="form.condition">
            <option value="greater_than">より大きい</option>
            <option value="less_than">より小さい</option>
          </select></label
        ><label class="field"
          ><span>しきい値</span
          ><input v-model.number="form.threshold" type="number" step="0.1" /></label
        ><label class="field"
          ><span>重要度</span
          ><select v-model="form.severity"><option value="info">情報</option><option value="warning">警告</option><option value="critical">重大</option></select></label
        ><label class="field"
          ><span>Webhook URL</span><input v-model="form.webhook_url" type="url" /></label
        ><label class="field"
          ><span>Webhook形式</span
          ><select v-model="form.webhook_format"><option value="json">JSON</option><option value="discord">Discord</option><option value="slack">Slack</option></select></label
        ><label class="check"><input v-model="form.is_enabled" type="checkbox" />ルールを有効にする</label
        ><button class="button" type="submit">追加</button>
      </form>
    </div>
  </section>
</template>

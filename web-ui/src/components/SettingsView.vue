<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { api, type JsonRecord } from '../api'
const endpoints = [
  ['スキャン', '/scan-config'],
  ['チューナー', '/tuner-config'],
  ['プレビュー', '/preview-config'],
  ['TS置換', '/tsreplace-config'],
] as const
const selected = ref(endpoints[0][1])
const body = ref('')
const message = ref('')
const error = ref('')
async function load() {
  try {
    body.value = JSON.stringify(await api<JsonRecord>(selected.value), null, 2)
    error.value = ''
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}
async function save() {
  try {
    await api(selected.value, { method: 'POST', body: JSON.stringify(JSON.parse(body.value)) })
    message.value = '保存しました'
    error.value = ''
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}
onMounted(load)
</script>
<template>
  <section class="view">
    <div class="view-heading">
      <div>
        <h2>設定</h2>
        <p>サーバー設定をJSON形式で確認・編集</p>
      </div>
      <button class="button" @click="save">保存</button>
    </div>
    <label class="field"
      ><span>設定カテゴリ</span
      ><select v-model="selected" @change="load">
        <option
          v-for="item in endpoints"
          :key="item[1]"
          :value="item[1]"
          v-text="item[0]"
        ></option></select></label
    ><label class="field"
      ><span>設定内容</span
      ><textarea v-model="body" class="json-editor" spellcheck="false"></textarea>
    </label>
    <p v-if="message" class="notice success" v-text="message"></p>
    <p v-if="error" class="notice error" role="alert" v-text="error"></p>
  </section>
</template>

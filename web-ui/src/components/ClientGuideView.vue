<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { api, unwrapArray, type JsonRecord } from '../api'
const targets = ref<JsonRecord[]>([])
const selected = ref('')
const info = ref<JsonRecord>({})
const error = ref('')
async function load() {
  try {
    targets.value = unwrapArray(await api('/client-view/targets'), ['targets', 'data'])
    if (!selected.value && targets.value[0])
      selected.value = String(targets.value[0].name ?? targets.value[0].tuner ?? '')
    await loadView()
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}
async function loadView() {
  if (selected.value)
    info.value = await api(`/client-view?tuner=${encodeURIComponent(selected.value)}`)
}
function file(kind: string) {
  location.href = `/api/client-view/files/${kind}?tuner=${encodeURIComponent(selected.value)}`
}
onMounted(load)
</script>
<template>
  <section class="view">
    <div class="view-heading">
      <div>
        <h2>クライアント設定</h2>
        <p>TVTest / EDCB向け設定ファイルを生成</p>
      </div>
    </div>
    <label class="field"
      ><span>接続先チューナー</span
      ><select v-model="selected" @change="loadView">
        <option
          v-for="(target, index) in targets"
          :key="index"
          :value="String(target.name ?? target.tuner ?? '')"
          v-text="String(target.display_name ?? target.name ?? target.tuner ?? '')"
        ></option></select
    ></label>
    <div class="actions">
      <button class="button secondary" @click="file('tvtest-ch2')">TVTest .ch2</button
      ><button class="button secondary" @click="file('chset4')">ChSet4</button
      ><button class="button secondary" @click="file('chset5')">ChSet5</button
      ><button class="button" @click="file('bundle')">まとめてダウンロード</button>
    </div>
    <pre class="code-block" v-text="JSON.stringify(info, null, 2)"></pre>
    <p v-if="error" class="notice error" role="alert" v-text="error"></p>
  </section>
</template>

<script setup lang="ts">
import mpegts from 'mpegts.js'
import { onBeforeUnmount, ref } from 'vue'

const sid = ref('')
const video = ref<HTMLVideoElement | null>(null)
const error = ref('')
const active = ref(false)
let player: ReturnType<typeof mpegts.createPlayer> | null = null

async function start() {
  stop()
  error.value = ''
  try {
    if (!mpegts.isSupported() || !video.value) {
      throw new Error('このブラウザでは再生できません')
    }
    const token = localStorage.getItem('recisdbApiToken')
    player = mpegts.createPlayer(
      {
        type: 'mpegts',
        isLive: true,
        url: `/api/stream/service/${encodeURIComponent(sid.value)}?profile=preview`,
      },
      {
        enableWorker: true,
        liveBufferLatencyChasing: true,
        headers: token ? { Authorization: `Bearer ${token}` } : {},
      },
    )
    player.attachMediaElement(video.value)
    player.load()
    await player.play()
    active.value = true
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
    stop()
  }
}

function stop() {
  player?.destroy()
  player = null
  active.value = false
  if (video.value) {
    video.value.removeAttribute('src')
    video.value.load()
  }
}

onBeforeUnmount(stop)
</script>

<template>
  <section class="player-panel">
    <div class="view-heading">
      <div>
        <h3>ブラウザプレビュー</h3>
        <p>サービスID（SID）を指定してH.264プレビューを再生</p>
      </div>
      <div class="actions">
        <button v-if="active" class="button danger" @click="stop">停止</button>
        <button v-else class="button" :disabled="!sid" @click="start">再生</button>
      </div>
    </div>
    <label class="field">
      <span>サービスID</span>
      <input v-model="sid" inputmode="numeric" placeholder="例: 1024" @keyup.enter="start" />
    </label>
    <video ref="video" class="preview-video" controls playsinline />
    <p v-if="error" class="notice error" role="alert" v-text="error" />
  </section>
</template>

<script setup lang="ts">
import mpegts from 'mpegts.js'
import DPlayer from 'dplayer'
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'

const props = defineProps<{
  /** When set, the player locks onto this SID and starts automatically. */
  initialSid?: string | number | null
}>()

const sid = ref(props.initialSid != null ? String(props.initialSid) : '')
const locked = props.initialSid != null
const container = ref<HTMLElement | null>(null)
const error = ref('')
const active = ref(false)
let dp: DPlayer | null = null
let mpegtsPlayer: ReturnType<typeof mpegts.createPlayer> | null = null

async function start() {
  stop()
  error.value = ''
  try {
    if (!mpegts.isSupported() || !container.value) {
      throw new Error('このブラウザでは再生できません')
    }
    const token = localStorage.getItem('recisdbApiToken')
    const url = `/api/stream/service/${encodeURIComponent(sid.value)}?profile=preview`
    // DPlayer delegates actual decoding to mpegts.js via customType, so the
    // existing Authorization-header handling keeps working unchanged.
    dp = new DPlayer({
      container: container.value,
      live: true,
      autoplay: false,
      preload: 'none',
      hotkey: true,
      screenshot: false,
      video: {
        url,
        type: 'customMpegts',
        customType: {
          customMpegts: (video) => {
            try {
              mpegtsPlayer = mpegts.createPlayer(
                {
                  type: 'mpegts',
                  isLive: true,
                  url,
                },
                {
                  enableWorker: true,
                  liveBufferLatencyChasing: true,
                  headers: token ? { Authorization: `Bearer ${token}` } : {},
                },
              )
              mpegtsPlayer.attachMediaElement(video)
              mpegtsPlayer.load()
              video.play().catch((cause: unknown) => {
                error.value = cause instanceof Error ? cause.message : String(cause)
                stop()
              })
            } catch (cause) {
              error.value = cause instanceof Error ? cause.message : String(cause)
              stop()
            }
          },
        },
      },
    })
    active.value = true
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
    stop()
  }
}

function stop() {
  mpegtsPlayer?.destroy()
  mpegtsPlayer = null
  dp?.destroy()
  dp = null
  active.value = false
}

if (locked) {
  onMounted(() => {
    void start()
  })
  watch(
    () => props.initialSid,
    (value) => {
      if (value == null) return
      sid.value = String(value)
      void start()
    },
  )
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
      <input
        v-model="sid"
        inputmode="numeric"
        placeholder="例: 1024"
        :readonly="locked"
        @keyup.enter="start"
      />
    </label>
    <div ref="container" class="preview-video" />
    <p v-if="error" class="notice error" role="alert" v-text="error" />
  </section>
</template>

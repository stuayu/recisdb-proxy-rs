<script setup lang="ts">
import mpegts from 'mpegts.js'
import DPlayer from 'dplayer'
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'

const props = defineProps<{
  /** When set, the player locks onto this SID and starts automatically. */
  initialSid?: string | number | null
  /**
   * NID paired with `initialSid`. SID alone isn't a unique service identity
   * (BS/BS4K reuse SIDs, terrestrial SIDs repeat across regions), so this is
   * sent as `&nid=` to disambiguate — without it the server 409s when the
   * SID is genuinely ambiguous instead of guessing a network.
   */
  initialNid?: string | number | null
}>()

const sid = ref(props.initialSid != null ? String(props.initialSid) : '')
const nid = ref(props.initialNid != null ? String(props.initialNid) : '')
const locked = props.initialSid != null
const container = ref<HTMLElement | null>(null)
const error = ref('')
const active = ref(false)
let dp: DPlayer | null = null
let mpegtsPlayer: ReturnType<typeof mpegts.createPlayer> | null = null

/// mpegts.js swallows the HTTP response body, so on failure re-fetch the
/// stream URL once to surface the server's human-readable reason (e.g.
/// "preview_encoder_config.enabled is false ...") instead of a black screen.
async function explainStreamError(url: string, token: string | null, fallback: string) {
  let message = `再生エラー: ${fallback}`
  try {
    const response = await fetch(url, {
      headers: token ? { Authorization: `Bearer ${token}` } : {},
    })
    if (!response.ok) {
      const body = (await response.text()).slice(0, 300)
      message = `プレビューを開始できません (HTTP ${response.status}): ${body || response.statusText}`
      if (/preview_encoder|command_path/i.test(body)) {
        message += '\nヒント: recisdb-proxy.toml の [preview] でエンコーダ(QSVEncC等)と前段(tsreadex)のパス設定が必要です。'
      }
    } else {
      void response.body?.cancel()
    }
  } catch {
    // keep fallback
  }
  error.value = message
  stop()
}

async function start() {
  stop()
  error.value = ''
  try {
    if (!mpegts.isSupported() || !container.value) {
      throw new Error('このブラウザでは再生できません')
    }
    const token = localStorage.getItem('recisdbApiToken')
    // by-sid: 放送のservice_idで解決する(/stream/service/:id はDB主キーなので使わない)。
    // SIDは網(NID)をまたぐと重複しうるので、分かっていれば必ずnid=を付けて曖昧さを
    // 排除する(省略時、サーバ側で複数網にまたがるSIDは409を返す)。
    let url = `/api/stream/service/by-sid/${encodeURIComponent(sid.value)}?profile=preview`
    if (nid.value !== '') {
      url += `&nid=${encodeURIComponent(nid.value)}`
    }
    // DPlayer delegates actual decoding to mpegts.js via customType, so the
    // existing Authorization-header handling keeps working unchanged.
    dp = new DPlayer({
      container: container.value,
      live: true,
      // DPlayer calls its own play() after construction and handles the
      // play() promise rejection internally. Calling video.play() ourselves
      // inside customType races DPlayer's init (pause()) and dies with
      // "The play() request was interrupted by a call to pause()".
      autoplay: true,
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
                  // Worker-mode fetch has environment-dependent failure
                  // modes (opaque "Exception" NetworkErrors); the ~2Mbps
                  // preview stream doesn't need a worker anyway.
                  enableWorker: false,
                  liveBufferLatencyChasing: true,
                  headers: token ? { Authorization: `Bearer ${token}` } : {},
                },
              )
              mpegtsPlayer.on(
                mpegts.Events.ERROR,
                (errType: string, detail: string, info: unknown) => {
                  let extra = ''
                  try {
                    extra = info ? ` ${JSON.stringify(info)}` : ''
                  } catch {
                    // ignore
                  }
                  void explainStreamError(url, token, `${errType} (${detail})${extra}`)
                },
              )
              mpegtsPlayer.attachMediaElement(video)
              mpegtsPlayer.load()
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
    () => [props.initialSid, props.initialNid] as const,
    ([sidValue, nidValue]) => {
      if (sidValue == null) return
      sid.value = String(sidValue)
      nid.value = nidValue != null ? String(nidValue) : ''
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
    <div class="preview-fields">
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
      <label class="field">
        <span>NID（任意・網をまたぐSIDの重複解消用）</span>
        <input
          v-model="nid"
          inputmode="numeric"
          placeholder="例: 4"
          :readonly="locked"
          @keyup.enter="start"
        />
      </label>
    </div>
    <div ref="container" class="preview-video" />
    <p v-if="error" class="notice error preserve-lines" role="alert" v-text="error" />
  </section>
</template>

<style scoped>
/* Two `.field`s side by side on wide screens; wraps to stacked full-width
   fields on narrow ones instead of overflowing (CLAUDE.md: no fixed px,
   must not break at ~360-430px phone widths). */
.preview-fields {
  display: flex;
  flex-wrap: wrap;
  gap: 0 16px;
}

.preview-fields .field {
  flex: 1 1 160px;
  min-width: 0;
}
</style>

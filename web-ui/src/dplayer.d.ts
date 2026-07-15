/**
 * Minimal ambient types for the `dplayer` package (no upstream `.d.ts`).
 * Only the surface used by PreviewPlayer.vue is declared.
 * https://github.com/tsukumijima/DPlayer
 */
declare module 'dplayer' {
  export interface DPlayerVideoOptions {
    url: string
    type?: string
    pic?: string
    thumbnails?: string
    customType?: Record<string, (video: HTMLVideoElement, player: DPlayer) => void>
  }

  export interface DPlayerOptions {
    container: HTMLElement
    live?: boolean
    autoplay?: boolean
    theme?: string
    lang?: string
    screenshot?: boolean
    hotkey?: boolean
    preload?: 'none' | 'metadata' | 'auto'
    volume?: number
    playbackSpeed?: number[]
    video: DPlayerVideoOptions
    danmaku?: false
  }

  export default class DPlayer {
    constructor(options: DPlayerOptions)
    video: HTMLVideoElement
    play(): void
    pause(): void
    seek(time: number): void
    destroy(): void
    on(event: string, handler: (...args: unknown[]) => void): void
  }
}

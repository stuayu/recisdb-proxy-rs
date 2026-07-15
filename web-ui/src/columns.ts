import { computed, ref } from 'vue'

export type ColumnDef = { key: string; label: string }

/**
 * テーブル列の表示/非表示状態を管理する。表示中の列キー一覧を
 * localStorage(storageKey)に保存し、次回訪問時も維持する。
 * 保存後に列定義が増えた場合、その新列は既定で非表示になる点は
 * 既存実装(OverviewView)と同じ挙動。
 */
export function useColumnVisibility(
  storageKey: string,
  all: () => ColumnDef[],
  defaults?: () => string[],
) {
  function load(): string[] | null {
    if (!storageKey) return null
    const raw = localStorage.getItem(storageKey)
    if (!raw) return null
    try {
      const parsed: unknown = JSON.parse(raw)
      if (Array.isArray(parsed)) {
        const valid = parsed.filter((key): key is string => typeof key === 'string')
        if (valid.length) return valid
      }
    } catch {
      localStorage.removeItem(storageKey)
    }
    return null
  }
  const saved = ref<string[] | null>(load())
  const visibleKeys = computed(() => {
    const keys = all().map((column) => column.key)
    const selected = saved.value ?? defaults?.()
    if (!selected) return keys
    const selection = new Set(selected)
    const filtered = keys.filter((key) => selection.has(key))
    return filtered.length ? filtered : keys
  })
  const visibleColumns = computed(() => {
    const visible = new Set(visibleKeys.value)
    return all().filter((column) => visible.has(column.key))
  })
  function setColumn(key: string, checked: boolean) {
    const next = new Set(visibleKeys.value)
    if (checked) next.add(key)
    else next.delete(key)
    if (!next.size) return // 最低1列は残す
    saved.value = all()
      .map((column) => column.key)
      .filter((columnKey) => next.has(columnKey))
    if (storageKey) localStorage.setItem(storageKey, JSON.stringify(saved.value))
  }
  function resetColumns() {
    saved.value = null
    if (storageKey) localStorage.removeItem(storageKey)
  }
  function isVisible(key: string) {
    return visibleKeys.value.includes(key)
  }
  return { visibleColumns, visibleKeys, isVisible, setColumn, resetColumns }
}

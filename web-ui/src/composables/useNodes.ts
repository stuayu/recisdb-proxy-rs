import { ref } from 'vue'
import { api } from '../api'
import type { NodeEntry, NodesResponse, ProbeResponse } from '../components/nodes/types'

export function useNodes() {
  const data = ref<NodesResponse | null>(null)
  const loading = ref(false)
  const error = ref('')
  const message = ref('')
  const probes = ref<Record<string, ProbeResponse>>({})
  const probing = ref<string | null>(null)

  async function load() {
    loading.value = true
    try {
      data.value = await api<NodesResponse>('/nodes')
      error.value = ''
    } catch (cause) {
      error.value = cause instanceof Error ? cause.message : String(cause)
    } finally {
      loading.value = false
    }
  }
  async function probe(entry: NodeEntry) {
    probing.value = entry.node.node_id
    try {
      probes.value[entry.node.node_id] = await api<ProbeResponse>(
        `/nodes/${encodeURIComponent(entry.node.node_id)}/probe`,
        {
          method: 'POST',
          body: JSON.stringify({ bitrate_bps: 20_000_000, download_bytes: 1_048_576 }),
        },
      )
      error.value = ''
    } catch (cause) {
      error.value = cause instanceof Error ? cause.message : String(cause)
    } finally {
      probing.value = null
    }
  }
  return { data, loading, error, message, probes, probing, load, probe }
}

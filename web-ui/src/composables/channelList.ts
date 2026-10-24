export type ChannelSort = { key: string; descending: boolean }

export function pageSizeChangeAction(currentPage: number) {
  return { page: 1, reload: currentPage === 1 }
}

export function buildChannelListParams(options: {
  page: number
  pageSize: number
  query: string
  sorts: ChannelSort[]
}) {
  const [first] = options.sorts
  const params = new URLSearchParams({
    limit: String(options.pageSize),
    offset: String((options.page - 1) * options.pageSize),
    sort: first?.key ?? 'channel_name',
    desc: String(first?.descending ?? false),
  })
  for (const [index, sort] of options.sorts.slice(1, 3).entries()) {
    if (!sort.key) continue
    const position = index + 2
    params.set(`sort${position}`, sort.key)
    params.set(`desc${position}`, String(sort.descending))
  }
  if (options.query.trim()) params.set('q', options.query.trim())
  return params
}

export function createLatestRequestGate() {
  let latestRequest = 0
  return {
    begin: () => ++latestRequest,
    isCurrent: (request: number) => request === latestRequest,
  }
}

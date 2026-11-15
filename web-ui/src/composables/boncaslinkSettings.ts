export type BoncaslinkNetworkReaderEdit = {
  nid: string
  reader: string
  custom: boolean
}

export function boncaslinkNetworkReaderPayload(rows: readonly BoncaslinkNetworkReaderEdit[]) {
  return rows.map((row) => ({
    nid: row.nid.trim().toUpperCase(),
    reader: row.custom
      ? `boncaslink://${row.reader.replace(/^boncaslink:\/\//, '')}`
      : row.reader,
  }))
}

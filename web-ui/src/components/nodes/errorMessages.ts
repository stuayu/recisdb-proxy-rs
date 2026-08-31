export function nodeErrorMessage(code: string | null, message: string): string {
  if (code === 'node_not_paired')
    return 'このPCとはまだ接続設定が完了していません。接続設定を開始してください。'
  if (code === 'upstream_error')
    return '相手PCに接続できません。通信方法または手動接続を確認してください。'
  return message
}

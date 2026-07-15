/** APIレスポンスの英語キー → 画面表示用の日本語ヘッダー。 */
export const columnLabels: Record<string, string> = {
  // 共通
  id: 'ID',
  name: '名前',
  created_at: '作成日時',
  updated_at: '更新日時',
  // スキャン履歴
  bon_driver_id: 'BonDriver ID',
  scan_time: 'スキャン日時',
  channel_count: 'チャンネル数',
  success: '結果',
  error_message: 'エラー内容',
  // セッション履歴
  session_id: 'セッションID',
  client_address: 'クライアント',
  tuner_path: 'チューナー',
  channel_info: 'チャンネル情報',
  channel_name: 'チャンネル名',
  started_at: '開始日時',
  ended_at: '終了日時',
  duration_secs: '視聴時間',
  packets_sent: '送信パケット',
  packets_dropped: 'ドロップ',
  packets_scrambled: 'スクランブル',
  packets_error: 'エラー',
  bytes_sent: '送信量',
  average_bitrate_mbps: '平均ビットレート',
  average_signal_level: '平均信号レベル',
  disconnect_reason: '切断理由',
  loss_summary: '損失サマリ',
  stream_class: 'ストリーム種別',
  // クライアント設定ガイド
  index: '番号',
  physical: '物理チャンネル',
}

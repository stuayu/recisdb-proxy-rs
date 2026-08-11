/**
 * ARIB STD-B10 で固定割り当てされている PID の名称。
 * 生の数値だけでは何のストリームか分からないので、TVTest のように補記するために使う。
 * ここに無い PID は PMT で番組ごとに割り当てられる（映像・音声・字幕・データ）ため、
 * TS を解析しないと判別できない。
 */
const FIXED_PIDS: Record<number, string> = {
  0x0000: 'PAT（番組対応表）',
  0x0001: 'CAT（限定受信情報）',
  0x0002: 'TSDT（TS記述）',
  0x0010: 'NIT（ネットワーク情報）',
  0x0011: 'SDT / BAT（サービス・ブーケ情報）',
  0x0012: 'EIT（番組情報）',
  0x0013: 'RST（進行状態）',
  0x0014: 'TDT / TOT（時刻・日付）',
  0x0017: 'DCT（ダウンロード制御）',
  0x001e: 'DIT（分割情報）',
  0x001f: 'SIT（選択情報）',
  0x0020: 'LIT（ローカルイベント情報）',
  0x0021: 'ERT（イベント関係）',
  0x0022: 'PCAT（番組配列情報）',
  0x0023: 'SDTT（ソフトウェアダウンロード）',
  0x0024: 'BIT（ブロードキャスタ情報）',
  0x0025: 'NBIT / LDT（掲示板・リンク記述）',
  0x0026: 'EIT（番組情報・拡張）',
  0x0027: 'EIT（番組情報・拡張）',
  0x0028: 'SDTT（ソフトウェアダウンロード）',
  0x0029: 'CDT（共通データ）',
  0x002e: 'AMT（アドレスマップ）',
  0x1fff: 'NULL（ヌルパケット）',
}

/** PID の名称。固定割り当てでなければ null。 */
export function pidName(pid: number): string | null {
  return FIXED_PIDS[pid] ?? null
}

/** 数値・文字列いずれの表現でも PID 値として解釈する（'0x100' も受ける）。 */
export function toPid(value: unknown): number | null {
  const num =
    typeof value === 'number'
      ? value
      : typeof value === 'string' && value.trim() !== ''
        ? Number(value)
        : NaN
  if (!Number.isFinite(num) || !Number.isInteger(num) || num < 0 || num > 0x1fff) return null
  return num
}

/** 表示用の PID 文字列。例: `0x0012 (18) EIT（番組情報）` */
export function formatPid(value: unknown): string {
  const pid = toPid(value)
  if (pid == null) return value == null ? '—' : String(value)
  const hex = `0x${pid.toString(16).toUpperCase().padStart(4, '0')}`
  const name = pidName(pid)
  return name ? `${hex} ${name}` : hex
}

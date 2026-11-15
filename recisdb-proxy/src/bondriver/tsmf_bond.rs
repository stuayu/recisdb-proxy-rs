//! 複数搬送波伝送 (拡張 TSMF) の合成。
//!
//! ケーブルテレビの BS8K などは、1搬送波 (256QAM 約 38 Mbps) に収まらない TLV
//! ストリームを複数の搬送波に分けて送る。各搬送波は拡張 TSMF で、1フレーム =
//! ヘッダー1スロット + データ 52 スロット (各 188 バイト)。合成の規則は
//! 情報通信審議会 放送システム委員会「ケーブルテレビにおける超高精細度テレビジョン
//! 放送の導入に関する伝送方式の高度化提案」4.7 節による:
//!
//! - 各搬送波のスーパーフレーム (256QAM は4フレーム) の先頭
//!   (`frame_position == 0`) で搬送波間の位相をそろえる。
//! - スーパーフレームを `number_of_frames` スロットずつのサブフレームに区切り、
//!   サブフレーム内はスロット位置の小さい順、同じ位置なら `carrier_sequence` の
//!   小さい順に並べる。変調方式が同じ搬送波だけの群では、各搬送波の同じ番号の
//!   スロットを搬送波順に1つずつ取る並びと同じになる (この実装はこの場合だけ扱う)。
//! - 並べたスロットのうち、TLV ストリームに割り当てられたものが分割 TLV パケット
//!   (PID 0x002D、3バイトのヘッダー、先頭パケットはポインター付き) になっている。
//!
//! 受信機ごとにデータの届く時刻はずれる (USB 転送単位・バッファー)。
//! `frame_position` は 0〜3 しかないので、それだけでは 4フレーム単位のずれを
//! 区別できない。候補の組み合わせごとに合成し、TLV の長さが次の TLV の開始位置と
//! 一致する割合で正しい位相を選ぶ。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const PACKET: usize = 188;
pub const SLOTS: usize = 52;
const FRAME_BYTES: usize = SLOTS * PACKET;
const TSMF_PID: u16 = 0x002f;
const DIVIDED_TLV_PID: u16 = 0x002d;

/// 位相合わせの評価に使うフレーム数 (2スーパーフレーム)。
const SCORE_FRAMES: usize = 8;
/// 位相合わせで探す受信時刻の幅。USB 転送1回 (816 パケット) は約 30 ms。
const ALIGN_WINDOW: Duration = Duration::from_millis(200);
/// 位相が合ったとみなす、TLV の連結が正しかった件数の下限。
const MIN_GOOD_LINKS: usize = 20;
/// 1搬送波あたりに溜めるフレーム数の上限 (約 5 秒)。
const MAX_BUFFERED_FRAMES: usize = 2400;
/// TLV パケットの最大長 (ヘッダー4バイト + 長さ 16 ビット)。
const TLV_MAX: usize = 4 + 0xffff;

/// 拡張 TSMF 多重フレームヘッダーのうち、合成に使う項目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtHeader {
    pub group_id: u8,
    pub number_of_carriers: u8,
    pub carrier_sequence: u8,
    pub number_of_frames: u8,
    pub frame_position: u8,
    /// TLV を運ぶ相対ストリーム番号 (1〜15)。
    pub tlv_stream: Option<u8>,
    /// スロットごとの相対ストリーム番号 (0 は未割り当て)。
    pub slot_stream: [u8; SLOTS],
}

impl ExtHeader {
    /// 複数搬送波伝送の搬送波か。通常の TSMF はこの領域が全ビット 1。
    pub fn is_multi_carrier(&self) -> bool {
        self.group_id != 0
            && self.group_id != 0xff
            && (2..0xff).contains(&self.number_of_carriers)
            && self.carrier_sequence >= 1
            && self.carrier_sequence <= self.number_of_carriers
    }
}

fn pid(p: &[u8]) -> u16 {
    ((p[1] as u16 & 0x1f) << 8) | p[2] as u16
}

/// MPEG-2 CRC32 (多項式 0x04C11DB7、初期値 0xFFFFFFFF)。
fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// 188 バイトのパケットが TSMF ヘッダーなら解析する。
pub fn parse_header(p: &[u8]) -> Option<ExtHeader> {
    if p.len() < PACKET || p[0] != 0x47 || pid(p) != TSMF_PID || p[3] & 0xf0 != 0x10 {
        return None;
    }
    let sync = ((p[4] as u16) << 8 | p[5] as u16) & 0x1fff;
    if sync != 0x1a86 && sync != 0x0579 {
        return None;
    }
    if crc32_mpeg2(&p[4..PACKET]) != 0 {
        return None;
    }
    let status = (p[7] as u16) << 8 | p[8] as u16;
    let types = (p[125] as u16) << 8 | p[126] as u16;
    let tlv_stream = (0..15u8).find(|&i| {
        let bit = 0x8000u16 >> i;
        status & bit != 0 && types & bit == 0
    });
    let mut slot_stream = [0u8; SLOTS];
    for (i, s) in slot_stream.iter_mut().enumerate() {
        let b = p[73 + i / 2];
        *s = if i & 1 == 0 { b >> 4 } else { b & 0x0f };
    }
    Some(ExtHeader {
        group_id: p[127],
        number_of_carriers: p[128],
        carrier_sequence: p[129],
        number_of_frames: p[130] >> 4,
        frame_position: p[130] & 0x0f,
        tlv_stream: tlv_stream.map(|i| i + 1),
        slot_stream,
    })
}

/// 生 TS から最初に見つかった TSMF ヘッダーを返す (188 バイト境界は自動で探す)。
pub fn find_header(data: &[u8]) -> Option<ExtHeader> {
    let start = (0..PACKET.min(data.len()))
        .find(|&o| (0..3).all(|k| data.get(o + k * PACKET).copied() == Some(0x47)))?;
    data[start..].chunks_exact(PACKET).find_map(parse_header)
}

/// 生 TS の最後の TSMF ヘッダーを返す。選局直後は前の周波数のデータが
/// 先頭に少し残るので、判定には後ろのヘッダーを使う。
pub fn find_last_header(data: &[u8]) -> Option<ExtHeader> {
    let start = (0..PACKET.min(data.len()))
        .find(|&o| (0..3).all(|k| data.get(o + k * PACKET).copied() == Some(0x47)))?;
    data[start..]
        .chunks_exact(PACKET)
        .rev()
        .find_map(parse_header)
}

struct Frame {
    header: ExtHeader,
    slots: Vec<u8>,
    arrival: Instant,
    /// 直前のフレームとの間でデータが欠けた。
    gap_before: bool,
}

#[derive(Default)]
struct CarrierInput {
    partial: Vec<u8>,
    building: Option<Frame>,
    frames: VecDeque<Frame>,
    gap: bool,
}

impl CarrierInput {
    fn push(&mut self, bytes: &[u8], now: Instant) {
        self.partial.extend_from_slice(bytes);
        let mut off = 0;
        while self.partial.len() - off >= PACKET {
            if self.partial[off] != 0x47 {
                // 188 バイト境界を見失った。次に 0x47 が2回続く位置からやり直す。
                let len = self.partial.len();
                let next = (off + 1..len).find(|&i| {
                    self.partial[i] == 0x47
                        && (i + PACKET >= len || self.partial[i + PACKET] == 0x47)
                });
                self.gap = true;
                self.building = None;
                match next {
                    Some(i) => {
                        off = i;
                        continue;
                    }
                    None => {
                        off = len;
                        break;
                    }
                }
            }
            let (s, e) = (off, off + PACKET);
            off = e;
            let packet: [u8; PACKET] = self.partial[s..e].try_into().unwrap();
            self.packet(&packet, now);
        }
        self.partial.drain(..off);
    }

    fn packet(&mut self, p: &[u8; PACKET], now: Instant) {
        if let Some(header) = parse_header(p) {
            if let Some(frame) = self.building.take() {
                if frame.slots.len() == FRAME_BYTES {
                    self.frames.push_back(frame);
                } else {
                    self.gap = true;
                }
            }
            if self.frames.len() > MAX_BUFFERED_FRAMES {
                self.frames.pop_front();
                if let Some(front) = self.frames.front_mut() {
                    front.gap_before = true;
                }
            }
            self.building = Some(Frame {
                header,
                slots: Vec::with_capacity(FRAME_BYTES),
                arrival: now,
                gap_before: std::mem::take(&mut self.gap),
            });
            return;
        }
        match self.building.as_mut() {
            Some(frame) if frame.slots.len() < FRAME_BYTES => frame.slots.extend_from_slice(p),
            Some(_) => {
                // ヘッダーが欠けて 52 スロットを超えた。
                self.building = None;
                self.gap = true;
            }
            None => {}
        }
    }
}

/// 分割 TLV パケットから TLV パケット列を組み立てる。
#[derive(Default)]
struct TlvAssembler {
    cur: Vec<u8>,
    active: bool,
    good: u64,
    bad: u64,
}

impl TlvAssembler {
    fn reset(&mut self) {
        self.cur.clear();
        self.active = false;
    }

    fn feed(&mut self, p: &[u8], out: &mut Vec<u8>) {
        if p[0] != 0x47 || p[1] & 0x80 != 0 || pid(p) != DIVIDED_TLV_PID {
            // 誤り訂正できなかったスロットは中身を信用できない。次の先頭まで捨てる。
            self.reset();
            return;
        }
        if p[1] & 0x40 != 0 {
            let start = 4 + p[3] as usize;
            if start >= PACKET {
                self.reset();
                return;
            }
            if self.active {
                self.cur.extend_from_slice(&p[4..start]);
                self.emit(out);
            }
            self.cur.clear();
            self.cur.extend_from_slice(&p[start..]);
            self.active = true;
        } else if self.active {
            self.cur.extend_from_slice(&p[3..]);
            if self.cur.len() > 2 * TLV_MAX {
                self.reset();
            }
        }
    }

    /// 次の先頭で区切られた TLV 列を、長さがちょうど合うときだけ出力する。
    fn emit(&mut self, out: &mut Vec<u8>) {
        if self.cur.is_empty() {
            return;
        }
        let mut q = 0;
        while q + 4 <= self.cur.len() && self.cur[q] == 0x7f {
            q += 4 + ((self.cur[q + 2] as usize) << 8 | self.cur[q + 3] as usize);
        }
        if q == self.cur.len() {
            out.extend_from_slice(&self.cur);
            self.good += 1;
        } else {
            self.bad += 1;
        }
    }
}

/// スロット列を合成したときに TLV の長さと次の先頭位置が合う件数と合わない件数。
fn score_links(packets: &[&[u8]]) -> (usize, usize) {
    let mut buf = Vec::with_capacity(packets.len() * 185);
    let mut starts = Vec::new();
    for p in packets {
        if pid(p) != DIVIDED_TLV_PID {
            buf.extend_from_slice(&[0; 185]);
            continue;
        }
        if p[1] & 0x40 != 0 {
            starts.push(buf.len() + p[3] as usize);
            buf.extend_from_slice(&p[4..]);
        } else {
            buf.extend_from_slice(&p[3..]);
        }
    }
    let (mut good, mut bad) = (0, 0);
    let Some(&last) = starts.last() else {
        return (0, 0);
    };
    for &s in &starts[..starts.len() - 1] {
        let mut q = s;
        let mut linked = false;
        for _ in 0..64 {
            if q + 4 > buf.len() || buf[q] != 0x7f {
                break;
            }
            q += 4 + ((buf[q + 2] as usize) << 8 | buf[q + 3] as usize);
            if q > s && starts.binary_search(&q).is_ok() {
                linked = true;
                break;
            }
            if q >= last {
                break;
            }
        }
        if linked {
            good += 1;
        } else {
            bad += 1;
        }
    }
    (good, bad)
}

/// 合成の状況 (ログ用)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BondStats {
    pub synced: bool,
    pub resyncs: u64,
    pub tlv_good: u64,
    pub tlv_bad: u64,
}

/// 複数搬送波の生 TS (TSMF ヘッダーを含む全スロット) を受け取り、TLV を出力する。
pub struct BondCombiner {
    carriers: Vec<CarrierInput>,
    synced: bool,
    tlv: TlvAssembler,
    out: Vec<u8>,
    resyncs: u64,
    /// 位相合わせを最後に試した第1搬送波のフレームの受信時刻。
    last_tried: Option<Instant>,
    /// 同期後の直近の TLV 判定 (良, 悪)。ずれの検出に使う。
    window: (u64, u64),
}

impl BondCombiner {
    /// `carriers` は `carrier_sequence` の順 (第1搬送波が先頭)。
    pub fn new(carriers: usize) -> Self {
        Self {
            carriers: (0..carriers).map(|_| CarrierInput::default()).collect(),
            synced: false,
            tlv: TlvAssembler::default(),
            out: Vec::new(),
            resyncs: 0,
            last_tried: None,
            window: (0, 0),
        }
    }

    pub fn push(&mut self, carrier: usize, bytes: &[u8], now: Instant) {
        self.carriers[carrier].push(bytes, now);
    }

    /// 溜まったフレームを合成し、出力できた TLV のバイト数を返す。
    pub fn pump(&mut self) -> usize {
        if !self.synced {
            self.try_sync();
        }
        while self.synced && self.carriers.iter().all(|c| !c.frames.is_empty()) {
            let pos = self.carriers[0].frames[0].header.frame_position;
            let lost = self.carriers.iter().any(|c| {
                let f = &c.frames[0];
                f.gap_before || f.header.frame_position != pos
            });
            if lost {
                self.lose_sync();
                break;
            }
            let frames: Vec<Frame> = self
                .carriers
                .iter_mut()
                .map(|c| c.frames.pop_front().unwrap())
                .collect();
            let Some(stream) = frames[0].header.tlv_stream else {
                continue;
            };
            let (good0, bad0) = (self.tlv.good, self.tlv.bad);
            for k in 0..SLOTS {
                for f in &frames {
                    if f.header.slot_stream[k] == stream {
                        let p = &f.slots[k * PACKET..(k + 1) * PACKET];
                        self.tlv.feed(p, &mut self.out);
                    }
                }
            }
            self.window.0 += self.tlv.good - good0;
            self.window.1 += self.tlv.bad - bad0;
            if self.window.0 + self.window.1 >= 400 {
                // 位相がずれると TLV がほぼ全部壊れる。半分以上なら合わせ直す。
                if self.window.1 * 2 > self.window.0 + self.window.1 {
                    self.lose_sync();
                    break;
                }
                self.window = (0, 0);
            }
        }
        self.out.len()
    }

    /// 合成済みの TLV を `buf` へ取り出す。
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.out.len());
        buf[..n].copy_from_slice(&self.out[..n]);
        self.out.drain(..n);
        n
    }

    pub fn pending(&self) -> usize {
        self.out.len()
    }

    pub fn clear(&mut self) {
        for c in &mut self.carriers {
            *c = CarrierInput::default();
        }
        self.out.clear();
        self.tlv.reset();
        self.synced = false;
        self.last_tried = None;
        self.window = (0, 0);
    }

    pub fn stats(&self) -> BondStats {
        BondStats {
            synced: self.synced,
            resyncs: self.resyncs,
            tlv_good: self.tlv.good,
            tlv_bad: self.tlv.bad,
        }
    }

    fn lose_sync(&mut self) {
        self.synced = false;
        self.resyncs += 1;
        self.tlv.reset();
        self.window = (0, 0);
        // 同じ位置で再び合わせ直さないよう、先頭のフレームを1つずつ捨てる。
        for c in &mut self.carriers {
            c.frames.pop_front();
        }
    }

    /// 位相合わせの候補 (スーパーフレーム先頭で、後ろに評価用のフレームが
    /// 欠けなく続くもの) の位置。
    fn candidates(c: &CarrierInput) -> Vec<usize> {
        let n = c.frames.len();
        (0..n.saturating_sub(SCORE_FRAMES - 1))
            .filter(|&i| {
                c.frames[i].header.frame_position == 0
                    && (i + 1..i + SCORE_FRAMES).all(|j| !c.frames[j].gap_before)
            })
            .collect()
    }

    fn try_sync(&mut self) {
        let n = self.carriers.len();
        let lists: Vec<Vec<usize>> = self.carriers.iter().map(Self::candidates).collect();
        if lists.iter().any(|l| l.is_empty()) {
            self.trim();
            return;
        }
        // 他の搬送波が、第1搬送波の候補の前後 ALIGN_WINDOW を受信し終えている
        // 最も古い候補を1つだけ試す (毎回全候補を試すと重い)。
        let newest: Vec<Instant> = self
            .carriers
            .iter()
            .map(|c| c.frames.back().unwrap().arrival)
            .collect();
        let pick = lists[0].iter().copied().find(|&i| {
            let t = self.carriers[0].frames[i].arrival;
            self.last_tried.is_none_or(|last| t > last)
                && newest[1..].iter().all(|&nt| nt >= t + ALIGN_WINDOW)
        });
        let Some(a) = pick else {
            self.trim();
            return;
        };
        let t0 = self.carriers[0].frames[a].arrival;
        self.last_tried = Some(t0);
        let near: Vec<Vec<usize>> = (1..n)
            .map(|c| {
                lists[c]
                    .iter()
                    .copied()
                    .filter(|&i| {
                        let t = self.carriers[c].frames[i].arrival;
                        t + ALIGN_WINDOW >= t0 && t <= t0 + ALIGN_WINDOW
                    })
                    .collect()
            })
            .collect();
        if near.iter().any(|l| l.is_empty()) {
            self.trim();
            return;
        }

        let mut best: Option<(usize, usize, Vec<usize>)> = None;
        let mut idx = vec![0usize; n - 1];
        loop {
            let mut starts = vec![a];
            starts.extend(idx.iter().enumerate().map(|(c, &k)| near[c][k]));
            let (good, bad) = self.score(&starts);
            if best
                .as_ref()
                .is_none_or(|(g, b, _)| good as i64 - bad as i64 > *g as i64 - *b as i64)
            {
                best = Some((good, bad, starts));
            }
            // 次の組み合わせ。
            let mut c = 0;
            loop {
                if c == n - 1 {
                    break;
                }
                idx[c] += 1;
                if idx[c] < near[c].len() {
                    break;
                }
                idx[c] = 0;
                c += 1;
            }
            if c == n - 1 {
                break;
            }
        }

        if let Some((good, bad, starts)) = best {
            if good >= MIN_GOOD_LINKS && bad * 10 <= good {
                for (c, &s) in starts.iter().enumerate() {
                    self.carriers[c].frames.drain(..s);
                    self.carriers[c].frames[0].gap_before = false;
                }
                self.synced = true;
                self.tlv.reset();
                self.window = (0, 0);
                self.last_tried = None;
                return;
            }
        }
        self.trim();
    }

    fn score(&self, starts: &[usize]) -> (usize, usize) {
        let Some(stream) = self.carriers[0].frames[starts[0]].header.tlv_stream else {
            return (0, 0);
        };
        let mut packets: Vec<&[u8]> = Vec::with_capacity(SCORE_FRAMES * SLOTS * starts.len());
        for f in 0..SCORE_FRAMES {
            for k in 0..SLOTS {
                for (c, &s) in starts.iter().enumerate() {
                    let frame = &self.carriers[c].frames[s + f];
                    if frame.header.slot_stream[k] == stream {
                        packets.push(&frame.slots[k * PACKET..(k + 1) * PACKET]);
                    }
                }
            }
        }
        score_links(&packets)
    }

    /// 同期できないまま古いデータが溜まり続けないよう、2秒より古いフレームを捨てる。
    fn trim(&mut self) {
        let Some(newest) = self
            .carriers
            .iter()
            .filter_map(|c| c.frames.back().map(|f| f.arrival))
            .max()
        else {
            return;
        };
        let Some(limit) = newest.checked_sub(Duration::from_secs(2)) else {
            return;
        };
        for c in &mut self.carriers {
            while c.frames.front().is_some_and(|f| f.arrival < limit) {
                c.frames.pop_front();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 決定的な疑似乱数 (xorshift)。
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// TLV ストリーム: 大きな IP パケット相当と小さな NULL TLV を混ぜる。
    fn tlv_stream(rng: &mut Rng, bytes: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < bytes {
            let (kind, len) = if rng.next() % 3 == 0 {
                (0xff, (rng.next() % 120) as usize)
            } else {
                (0x03, 900 + (rng.next() % 700) as usize)
            };
            out.extend_from_slice(&[0x7f, kind, (len >> 8) as u8, len as u8]);
            out.extend((0..len).map(|_| rng.next() as u8));
        }
        out
    }

    /// TLV ストリームを分割 TLV パケット (PID 0x002D) に分ける。
    fn divide(stream: &[u8]) -> Vec<[u8; PACKET]> {
        // TLV の先頭位置。
        let mut starts = Vec::new();
        let mut q = 0;
        while q + 4 <= stream.len() {
            starts.push(q);
            q += 4 + ((stream[q + 2] as usize) << 8 | stream[q + 3] as usize);
        }
        let mut packets = Vec::new();
        let mut pos = 0;
        let mut si = 0;
        while pos < stream.len() {
            let mut p = [0u8; PACKET];
            p[0] = 0x47;
            p[2] = 0x2d;
            // この先 184 バイト以内に TLV の先頭があれば先頭パケットにする。
            while si < starts.len() && starts[si] < pos {
                si += 1;
            }
            let next = starts.get(si).copied().filter(|&s| s < pos + 184);
            if let Some(s) = next {
                p[1] = 0x40;
                p[3] = (s - pos) as u8;
                let n = (stream.len() - pos).min(184);
                p[4..4 + n].copy_from_slice(&stream[pos..pos + n]);
                pos += 184;
            } else {
                let n = (stream.len() - pos).min(185);
                p[3..3 + n].copy_from_slice(&stream[pos..pos + n]);
                pos += 185;
            }
            packets.push(p);
        }
        packets
    }

    fn header(group: u8, carriers: u8, seq: u8, pos: u8) -> [u8; PACKET] {
        let mut p = [0xffu8; PACKET];
        p[0] = 0x47;
        p[1] = 0x00;
        p[2] = 0x2f;
        p[3] = 0x10;
        p[4] = 0xfa;
        p[5] = 0x86;
        p[6] = 0xe2;
        p[7] = 0x80;
        p[8] = 0x01;
        p[73..99].fill(0x11);
        p[124] = 0xf0;
        p[125] = 0x7f;
        p[126] = 0xfe;
        p[127] = group;
        p[128] = carriers;
        p[129] = seq;
        p[130] = 0x40 | pos;
        let crc = crc32_mpeg2(&p[4..184]);
        p[184..].copy_from_slice(&crc.to_be_bytes());
        p
    }

    /// 合成済みの分割 TLV パケット列を `carriers` 波へ配り、各搬送波の生 TS を作る。
    fn split(packets: &[[u8; PACKET]], carriers: usize) -> Vec<Vec<u8>> {
        let per_frame = SLOTS * carriers;
        let frames = packets.len() / per_frame;
        let mut out = vec![Vec::new(); carriers];
        for f in 0..frames {
            for (c, o) in out.iter_mut().enumerate() {
                o.extend_from_slice(&header(8, carriers as u8, c as u8 + 1, (f % 4) as u8));
                for k in 0..SLOTS {
                    o.extend_from_slice(&packets[f * per_frame + k * carriers + c]);
                }
            }
        }
        out
    }

    fn run(
        skip_frames: &[usize],
        chunk: usize,
        drop_at: Option<u64>,
    ) -> (Vec<u8>, Vec<u8>, BondCombiner) {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let stream = tlv_stream(&mut rng, 6_000_000);
        let packets = divide(&stream);
        let raw = split(&packets, skip_frames.len());
        let mut comb = BondCombiner::new(raw.len());
        let t0 = Instant::now();
        // 搬送波ごとに受信開始がずれる: 先頭の何フレームかを受け取れていない。
        let mut offs: Vec<usize> = skip_frames
            .iter()
            .map(|&s| s * (SLOTS + 1) * PACKET + 5)
            .collect();
        let mut out = Vec::new();
        let mut buf = vec![0u8; 65536];
        let mut step = 0u64;
        while offs.iter().zip(&raw).any(|(o, r)| o < &r.len()) {
            let now = t0 + Duration::from_micros(step * 1000);
            for (c, r) in raw.iter().enumerate() {
                let end = (offs[c] + chunk).min(r.len());
                // 受信機のバッファーが溢れた想定: 第2搬送波の1チャンクを失う。
                if !(c == 1 && drop_at == Some(step)) {
                    comb.push(c, &r[offs[c]..end], now);
                }
                offs[c] = end;
            }
            comb.pump();
            loop {
                let n = comb.read(&mut buf);
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            step += 1;
        }
        (stream, out, comb)
    }

    #[test]
    fn parses_the_bs8k_header_captured_at_569_mhz() {
        let hex = "47002f1bfa86e28001b0e0000bffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff3ffffffe1111111111111111111111111111111111111111111111111111fffffffffffffffffffffffffffffffffffffffffffffffffff07ffe08030140ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff4b9471bc";
        let p: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let h = parse_header(&p).expect("valid header");
        assert_eq!(h.group_id, 8);
        assert_eq!(h.number_of_carriers, 3);
        assert_eq!(h.carrier_sequence, 1);
        assert_eq!(h.number_of_frames, 4);
        assert_eq!(h.frame_position, 0);
        assert_eq!(h.tlv_stream, Some(1));
        assert!(h.slot_stream.iter().all(|&s| s == 1));
        assert!(h.is_multi_carrier());
    }

    #[test]
    fn three_carriers_with_different_start_delays_recombine_exactly() {
        // 第2・第3搬送波はそれぞれ 9・22 フレーム遅れて受信を始めた。
        let (stream, out, comb) = run(&[0, 9, 22], 4096, None);
        assert!(comb.stats().synced, "{:?}", comb.stats());
        assert!(
            out.len() > stream.len() / 3,
            "out={} of {}",
            out.len(),
            stream.len()
        );
        // 出力は元のストリームの連続した一部で、TLV の先頭から始まる。
        let at = stream
            .windows(64)
            .position(|w| w == &out[..64])
            .expect("output must come from the stream");
        assert_eq!(&stream[at..at + out.len()], &out[..]);
        assert_eq!(comb.stats().tlv_bad, 0);
    }

    #[test]
    fn a_lost_chunk_on_one_carrier_resyncs_and_keeps_tlvs_whole() {
        let (_, out, comb) = run(&[0, 3, 5], 4096, Some(100));
        let stats = comb.stats();
        assert!(stats.synced && stats.resyncs >= 1, "{stats:?}");
        // 出力は TLV の区切りどおりに並び、壊れた TLV を含まない。
        let mut q = 0;
        while q < out.len() {
            assert_eq!(out[q], 0x7f, "broken TLV at {q}");
            q += 4 + ((out[q + 2] as usize) << 8 | out[q + 3] as usize);
        }
        assert_eq!(q, out.len());
        assert!(out.len() > 1_000_000, "out={}", out.len());
    }

    #[test]
    fn a_single_carrier_alone_does_not_sync() {
        // 第1搬送波だけを2波として与えても (同じ内容)、TLV はつながらない。
        let mut rng = Rng(7);
        let stream = tlv_stream(&mut rng, 600_000);
        let raw = split(&divide(&stream), 3);
        let mut comb = BondCombiner::new(2);
        let t0 = Instant::now();
        for (i, chunk) in raw[0].chunks(8192).enumerate() {
            let now = t0 + Duration::from_millis(i as u64);
            comb.push(0, chunk, now);
            comb.push(1, chunk, now);
            comb.pump();
        }
        assert_eq!(comb.pending(), 0);
        assert!(!comb.stats().synced);
    }

    #[test]
    fn find_header_locates_packet_boundary() {
        let mut data = vec![0u8; 7];
        for pos in 0..3 {
            data.extend_from_slice(&header(8, 3, 2, pos));
        }
        let h = find_header(&data).unwrap();
        assert_eq!((h.group_id, h.carrier_sequence), (8, 2));
    }
}

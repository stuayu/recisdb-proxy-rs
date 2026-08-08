//! Session-level (BNDP) checks against a running proxy and real hardware.
//!
//! These cover the parts of the tuner-selection redesign
//! (`docs/TUNER_PIPELINE_REDESIGN.md`) that only exist on the session path
//! and therefore cannot be reached through the HTTP streaming endpoints:
//! above all the **slot-permit handoff** on a same-DLL channel switch (P1b
//! §4), which is what lets a session retune a `max_instances = 1` driver at
//! all.
//!
//! Ignored by default — they need a proxy listening on `PROXY_ADDR` with a
//! scanned channel database and a real tuner behind it:
//!
//! ```text
//! cargo test -p recisdb-proxy --test bndp_hardware -- --ignored --nocapture
//! ```
//!
//! Overrides: `BNDP_ADDR` (default `127.0.0.1:40070`), `BNDP_GROUP`
//! (default `PX-MLT5PE`), `BNDP_SPACE` (default `0`), `BNDP_CHANNELS`
//! (default `0,1` — client-view channel indices, which must map to two
//! *different* physical channels for the switch to be meaningful).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use bytes::Bytes;
use recisdb_protocol::{
    codec::{decode_server_message, encode_client_message},
    ClientMessage, MessageType, ServerMessage, StreamClass, HEADER_SIZE, MAGIC,
};

fn addr() -> String {
    std::env::var("BNDP_ADDR").unwrap_or_else(|_| "127.0.0.1:40070".to_string())
}

fn group() -> String {
    std::env::var("BNDP_GROUP").unwrap_or_else(|_| "PX-MLT5PE".to_string())
}

struct Client {
    sock: TcpStream,
    buf: Vec<u8>,
}

impl Client {
    fn connect() -> Self {
        let sock = TcpStream::connect(addr()).expect("connect to the proxy");
        sock.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        Self { sock, buf: Vec::new() }
    }

    fn send(&mut self, msg: ClientMessage) {
        let frame = encode_client_message(&msg).expect("encode");
        self.sock.write_all(&frame).expect("send");
        self.sock.flush().unwrap();
    }

    /// Read one server message, skipping (and counting) TS data frames.
    fn recv_control(&mut self, ts_bytes: &mut usize) -> ServerMessage {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(msg) = self.try_take(ts_bytes) {
                return msg;
            }
            assert!(Instant::now() < deadline, "timed out waiting for a control message");
            let mut tmp = [0u8; 65536];
            let n = self.sock.read(&mut tmp).expect("read");
            assert!(n > 0, "server closed the connection");
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }

    /// Pull TS for `dur`, returning how many payload bytes arrived.
    fn drain_ts(&mut self, dur: Duration) -> usize {
        let mut ts = 0usize;
        let deadline = Instant::now() + dur;
        self.sock.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        while Instant::now() < deadline {
            while self.try_take(&mut ts).is_some() {}
            let mut tmp = [0u8; 65536];
            match self.sock.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(_) => {} // read timeout: just keep waiting until `deadline`
            }
        }
        while self.try_take(&mut ts).is_some() {}
        self.sock.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        ts
    }

    /// Decode one buffered frame. TS frames are counted into `ts_bytes` and
    /// skipped; anything else is returned.
    fn try_take(&mut self, ts_bytes: &mut usize) -> Option<ServerMessage> {
        loop {
            if self.buf.len() < HEADER_SIZE {
                return None;
            }
            assert_eq!(&self.buf[..MAGIC.len()], &MAGIC, "framing lost");
            let len = u32::from_le_bytes(self.buf[4..8].try_into().unwrap()) as usize;
            if self.buf.len() < HEADER_SIZE + len {
                return None;
            }
            let frame: Vec<u8> = self.buf.drain(..HEADER_SIZE + len).collect();
            let msg_type = MessageType::try_from(u16::from_le_bytes(
                frame[8..10].try_into().unwrap(),
            ))
            .expect("unknown message type");
            let payload = Bytes::copy_from_slice(&frame[HEADER_SIZE..]);
            match decode_server_message(msg_type, payload) {
                Ok(ServerMessage::TsData { data }) => {
                    *ts_bytes += data.len();
                    continue;
                }
                Ok(msg) => return Some(msg),
                Err(e) => panic!("decode failed: {e}"),
            }
        }
    }
}

/// Open a session, join `group`, tune to `(space, channel)` and start
/// streaming. Returns the live client, or the first failure as a string.
fn open_tuned_session(space: u32, channel: u32) -> Result<Client, String> {
    let mut ts = 0usize;
    let mut c = Client::connect();

    c.send(ClientMessage::Hello { version: 2, stream_class: StreamClass::View });
    match c.recv_control(&mut ts) {
        ServerMessage::HelloAck { success: true, .. } => {}
        other => return Err(format!("Hello: {other:?}")),
    }

    c.send(ClientMessage::OpenTunerWithGroup { group_name: group() });
    match c.recv_control(&mut ts) {
        ServerMessage::OpenTunerAck { success: true, .. } => {}
        ServerMessage::OpenTunerAck { error_code, .. } => {
            return Err(format!("OpenTuner error_code={error_code}"))
        }
        other => return Err(format!("OpenTuner: {other:?}")),
    }

    c.send(ClientMessage::SetChannelSpace { space, channel, priority: 0, exclusive: false });
    match c.recv_control(&mut ts) {
        ServerMessage::SetChannelSpaceAck { success: true, .. } => {}
        ServerMessage::SetChannelSpaceAck { error_code, .. } => {
            return Err(format!("SetChannelSpace error_code={error_code}"))
        }
        other => return Err(format!("SetChannelSpace: {other:?}")),
    }

    c.send(ClientMessage::StartStream);
    match c.recv_control(&mut ts) {
        ServerMessage::StartStreamAck { success: true, .. } => {}
        ServerMessage::StartStreamAck { error_code, .. } => {
            return Err(format!("StartStream error_code={error_code}"))
        }
        other => return Err(format!("StartStream: {other:?}")),
    }

    Ok(c)
}

/// Concurrent-session matrix. `BNDP_MATRIX` is a comma-separated list of
/// client-view channel indices, one per session, all started together:
///
/// - `0,1,2,3,4` — five distinct channels; each needs its own tuner, so this
///   is the "every receiver in use" case on a five-receiver group.
/// - `0,1,2,3,4,5` — one more than there are receivers; the last must fail
///   rather than push a driver over `max_instances`.
/// - `0,0,0,0,0` — one channel, five viewers; all must succeed by joining a
///   single reader (P1b §6).
///
/// Prints a per-session table and asserts only that *successes stream* and
/// that the count of successes matches `BNDP_MATRIX_EXPECT_OK` when set.
#[test]
#[ignore = "requires a running proxy with a scanned channel DB and real tuner hardware"]
fn matrix_concurrent_sessions() {
    let space: u32 = std::env::var("BNDP_SPACE").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let plan: Vec<u32> = std::env::var("BNDP_MATRIX")
        .unwrap_or_else(|_| "0,1,2,3,4".to_string())
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    let expect_ok: Option<usize> =
        std::env::var("BNDP_MATRIX_EXPECT_OK").ok().and_then(|v| v.parse().ok());

    // Start every session at once: staggering them would hide races in the
    // selection path, which is exactly what a matrix run is for.
    let handles: Vec<_> = plan
        .iter()
        .copied()
        .enumerate()
        .map(|(i, ch)| std::thread::spawn(move || (i, ch, open_tuned_session(space, ch))))
        .collect();

    let mut live = Vec::new();
    let mut results: Vec<(usize, u32, Result<(), String>)> = Vec::new();
    for h in handles {
        let (i, ch, r) = h.join().expect("session thread");
        match r {
            Ok(c) => {
                results.push((i, ch, Ok(())));
                live.push((i, ch, c));
            }
            Err(e) => results.push((i, ch, Err(e))),
        }
    }

    // Confirm the ones that got in actually receive.
    let mut streamed = Vec::new();
    for (i, ch, c) in live.iter_mut() {
        let got = c.drain_ts(Duration::from_secs(4));
        streamed.push((*i, *ch, got));
    }

    results.sort_by_key(|(i, _, _)| *i);
    println!("\n--- session matrix (space={space}) ---");
    for (i, ch, r) in &results {
        let bytes = streamed.iter().find(|(j, _, _)| j == i).map(|(_, _, b)| *b);
        match (r, bytes) {
            (Ok(()), Some(b)) => println!("  session {i}: channel {ch} → OK, {b} bytes"),
            (Ok(()), None) => println!("  session {i}: channel {ch} → OK (no drain)"),
            (Err(e), _) => println!("  session {i}: channel {ch} → REJECTED ({e})"),
        }
    }

    let ok = results.iter().filter(|(_, _, r)| r.is_ok()).count();
    println!("  → {ok}/{} sessions admitted", results.len());

    for (i, ch, got) in &streamed {
        assert!(
            *got > 188 * 200,
            "session {i} (channel {ch}) was admitted but delivered only {got} bytes"
        );
    }
    if let Some(expect) = expect_ok {
        assert_eq!(ok, expect, "expected exactly {expect} sessions to be admitted");
    }
}

/// A session must be able to switch channels on a `max_instances = 1`
/// driver. The old reader is not stopped until after the new one is in
/// place, so the switch only works if the session's own slot permit is
/// handed to the replacement instead of being released and re-acquired
/// (docs/TUNER_PIPELINE_REDESIGN.md P1b §4). Without the handoff the second
/// `SetChannelSpace` is rejected for lack of capacity.
#[test]
#[ignore = "requires a running proxy with a scanned channel DB and real tuner hardware"]
fn session_switches_channels_on_a_single_instance_driver() {
    let space: u32 = std::env::var("BNDP_SPACE").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let channels: Vec<u32> = std::env::var("BNDP_CHANNELS")
        .unwrap_or_else(|_| "0,1".to_string())
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    assert!(channels.len() >= 2, "need at least two channel indices to test a switch");

    let mut ts = 0usize;
    let mut c = Client::connect();

    c.send(ClientMessage::Hello { version: 2, stream_class: StreamClass::View });
    match c.recv_control(&mut ts) {
        ServerMessage::HelloAck { success, .. } => assert!(success, "Hello rejected"),
        other => panic!("expected HelloAck, got {other:?}"),
    }

    c.send(ClientMessage::OpenTunerWithGroup { group_name: group() });
    match c.recv_control(&mut ts) {
        ServerMessage::OpenTunerAck { success, error_code, .. } => {
            assert!(success, "OpenTuner failed (error_code={error_code})")
        }
        other => panic!("expected OpenTunerAck, got {other:?}"),
    }

    c.send(ClientMessage::SetChannelSpace {
        space,
        channel: channels[0],
        priority: 0,
        exclusive: false,
    });
    match c.recv_control(&mut ts) {
        ServerMessage::SetChannelSpaceAck { success, error_code } => {
            assert!(success, "initial SetChannelSpace failed (error_code={error_code})")
        }
        other => panic!("expected SetChannelSpaceAck, got {other:?}"),
    }

    c.send(ClientMessage::StartStream);
    match c.recv_control(&mut ts) {
        ServerMessage::StartStreamAck { success, error_code } => {
            assert!(success, "StartStream failed (error_code={error_code})")
        }
        other => panic!("expected StartStreamAck, got {other:?}"),
    }

    let first = c.drain_ts(Duration::from_secs(4));
    println!("channel {} → {} bytes", channels[0], first);
    assert!(first > 188 * 500, "no TS on the first channel ({first} bytes)");

    // The switch under test: same driver, different physical channel, while
    // streaming.
    for &ch in &channels[1..] {
        c.send(ClientMessage::SetChannelSpace { space, channel: ch, priority: 0, exclusive: false });
        let mut sink = 0usize;
        match c.recv_control(&mut sink) {
            ServerMessage::SetChannelSpaceAck { success, error_code } => assert!(
                success,
                "switching to channel {ch} failed (error_code={error_code}) — \
                 the session's slot permit was not carried over"
            ),
            other => panic!("expected SetChannelSpaceAck, got {other:?}"),
        }

        let got = c.drain_ts(Duration::from_secs(5));
        println!("channel {ch} → {got} bytes");
        assert!(got > 188 * 500, "no TS after switching to channel {ch} ({got} bytes)");
    }

    c.send(ClientMessage::StopStream);
}

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

    /// Pull TS until `min_bytes` have arrived, or `max_wait` elapses.
    ///
    /// Waiting for a byte count rather than for a fixed slice of wall clock
    /// matters on a cold start: `StartStreamAck` comes back as soon as the
    /// reader is *ready*, but the first bytes only appear after the BonDriver
    /// open settles and the session's prefill window (default 1 s) releases.
    /// A fixed 4 s drain measured zero on a freshly opened tuner while the
    /// server was demonstrably sending — the test was just looking too early.
    fn drain_ts(&mut self, max_wait: Duration) -> usize {
        self.drain_ts_until(188 * 500, max_wait)
    }

    fn drain_ts_until(&mut self, min_bytes: usize, max_wait: Duration) -> usize {
        let mut ts = 0usize;
        let deadline = Instant::now() + max_wait;
        self.sock.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let mut reads = 0usize;
        let mut errs: Vec<String> = Vec::new();
        let mut eof = false;
        while Instant::now() < deadline && ts < min_bytes {
            while self.try_take(&mut ts).is_some() {}
            let mut tmp = [0u8; 65536];
            match self.sock.read(&mut tmp) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => {
                    reads += 1;
                    self.buf.extend_from_slice(&tmp[..n]);
                }
                Err(e) => {
                    if errs.len() < 3 {
                        errs.push(format!("{:?}", e.kind()));
                    }
                }
            }
        }
        if ts == 0 {
            eprintln!(
                "    drain diagnostics: reads={reads} eof={eof} buffered={} errs={errs:?}",
                self.buf.len()
            );
        }
        while self.try_take(&mut ts).is_some() {}
        self.sock.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        ts
    }

    /// Has the server hung up? Drains anything still queued first: a session
    /// whose tuner was taken away still has frames sitting in the socket
    /// buffer, so "did bytes arrive" cannot tell a live stream from a dead
    /// one — only the EOF can.
    fn is_closed(&mut self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        self.sock.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut sink = 0usize;
        while Instant::now() < deadline {
            while self.try_take(&mut sink).is_some() {}
            let mut tmp = [0u8; 65536];
            match self.sock.read(&mut tmp) {
                Ok(0) => return true,
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(_) => {}
            }
        }
        false
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
    open_tuned_session_with_priority(space, channel, 0)
}

fn open_tuned_session_with_priority(
    space: u32,
    channel: u32,
    priority: i32,
) -> Result<Client, String> {
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

    c.send(ClientMessage::SetChannelSpace { space, channel, priority, exclusive: false });
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

    // Start every session at once by default: staggering hides races in the
    // selection path, which is exactly what a matrix run is for.
    // `BNDP_MATRIX_STAGGER_MS` introduces a delay between starts, which is
    // what distinguishes "arrived together, nothing to join yet" from
    // "arrived after someone already tuned this channel".
    let stagger_ms: u64 = std::env::var("BNDP_MATRIX_STAGGER_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let handles: Vec<_> = plan
        .iter()
        .copied()
        .enumerate()
        .map(|(i, ch)| {
            let delay = Duration::from_millis(stagger_ms * i as u64);
            std::thread::spawn(move || {
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                (i, ch, open_tuned_session(space, ch))
            })
        })
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
        let got = c.drain_ts(Duration::from_secs(20));
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

/// A recording-grade request must be able to take a receiver from a live
/// viewer once every receiver is busy — the deliberate consequence of
/// "never exceed `max_instances`" plus "a strictly higher priority wins"
/// (docs/TUNER_PIPELINE_REDESIGN.md P2b-3).
///
/// Fills the group with `BNDP_PRIO_FILL` (default `0,1,2,3,4`) viewers at
/// priority 0, then asks for `BNDP_PRIO_CHANNEL` (default `5`) at
/// `BNDP_PRIO` (default 200, recording-grade).
#[test]
#[ignore = "requires a running proxy with a scanned channel DB and real tuner hardware"]
fn a_higher_priority_request_displaces_a_viewer_when_every_receiver_is_busy() {
    let space: u32 = std::env::var("BNDP_SPACE").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let fill: Vec<u32> = std::env::var("BNDP_PRIO_FILL")
        .unwrap_or_else(|_| "0,1,2,3,4".to_string())
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    let target: u32 =
        std::env::var("BNDP_PRIO_CHANNEL").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let priority: i32 = std::env::var("BNDP_PRIO").ok().and_then(|v| v.parse().ok()).unwrap_or(200);

    // Fill every receiver with ordinary viewers, staggered so they settle on
    // distinct tuners rather than racing.
    let mut viewers = Vec::new();
    for &ch in &fill {
        match open_tuned_session(space, ch) {
            Ok(mut c) => {
                let got = c.drain_ts(Duration::from_secs(20));
                println!("viewer on channel {ch}: {got} bytes");
                assert!(got > 188 * 200, "filler viewer on channel {ch} never streamed");
                viewers.push(c);
            }
            Err(e) => panic!("could not fill the group: channel {ch}: {e}"),
        }
    }
    println!("group filled with {} viewers at priority 0", viewers.len());

    // Now the recording-grade request for a channel nobody is on.
    let mut rec = open_tuned_session_with_priority(space, target, priority)
        .unwrap_or_else(|e| panic!("priority {priority} request was refused: {e}"));
    let got = rec.drain_ts(Duration::from_secs(20));
    println!("priority {priority} request on channel {target}: {got} bytes");
    assert!(got > 188 * 200, "the displacing request was admitted but never streamed");

    // One of the viewers must have lost its tuner. The server drops such a
    // session as soon as the reader stops (P4), so its socket reports EOF.
    let mut displaced = 0;
    for (i, v) in viewers.iter_mut().enumerate() {
        if v.is_closed(Duration::from_secs(5)) {
            println!("viewer {i} was disconnected (its tuner was taken)");
            displaced += 1;
        }
    }
    assert!(
        displaced >= 1,
        "a receiver had to come from somewhere, but every viewer is still streaming"
    );
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

    let first = c.drain_ts(Duration::from_secs(20));
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

        let got = c.drain_ts(Duration::from_secs(20));
        println!("channel {ch} → {got} bytes");
        assert!(got > 188 * 500, "no TS after switching to channel {ch} ({got} bytes)");
    }

    c.send(ClientMessage::StopStream);
}

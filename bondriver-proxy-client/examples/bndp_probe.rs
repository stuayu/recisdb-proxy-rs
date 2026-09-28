//! Small cross-platform BNDP smoke client.
//!
//! This intentionally uses the public `Connection` API rather than the DLL
//! exports, so it can run on macOS/Linux while exercising the same Hello →
//! OpenTuner → SetChannelSpace → StartStream sequence as TVTest/EDCB.

use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use recisdb_protocol::StreamClass;
use BonDriver_NetworkProxy::{Connection, ConnectionConfig};

#[derive(Debug)]
struct Args {
    addr: String,
    space: u32,
    channel: u32,
    seconds: u64,
    priority: i32,
    exclusive: bool,
    out: Option<String>,
    tuner: String,
    record: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: bndp_probe --addr host:port --space N --channel N [--seconds 20] [--priority N] [--exclusive] [--out file.ts] [--tuner NAME] [--record]"
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut args = env::args().skip(1);
    let mut parsed = Args {
        addr: String::new(),
        space: 0,
        channel: 0,
        seconds: 20,
        priority: 0,
        exclusive: false,
        out: None,
        tuner: String::new(),
        record: false,
    };

    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next().unwrap_or_else(|| {
                eprintln!("missing value for {name}");
                usage()
            })
        };
        match arg.as_str() {
            "--addr" => parsed.addr = value("--addr"),
            "--space" => parsed.space = value("--space").parse().unwrap_or_else(|_| usage()),
            "--channel" => parsed.channel = value("--channel").parse().unwrap_or_else(|_| usage()),
            "--seconds" => parsed.seconds = value("--seconds").parse().unwrap_or_else(|_| usage()),
            "--priority" => {
                parsed.priority = value("--priority").parse().unwrap_or_else(|_| usage())
            }
            "--exclusive" => parsed.exclusive = true,
            "--tuner" => parsed.tuner = value("--tuner"),
            "--record" => parsed.record = true,
            "--out" => parsed.out = Some(value("--out")),
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    if parsed.addr.is_empty() {
        eprintln!("--addr is required");
        usage();
    }
    parsed
}

fn continuity_errors(data: &[u8], previous: &mut HashMap<u16, u8>) -> (u64, u64) {
    let mut sync_errors = 0;
    let mut cc_errors = 0;
    for packet in data.chunks_exact(188) {
        if packet[0] != 0x47 {
            sync_errors += 1;
            continue;
        }
        let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
        let adaptation = (packet[3] >> 4) & 0x03;
        let has_payload = adaptation == 1 || adaptation == 3;
        if !has_payload {
            continue;
        }
        let cc = packet[3] & 0x0f;
        if let Some(last) = previous.get(&pid) {
            if cc != ((*last + 1) & 0x0f) {
                cc_errors += 1;
            }
        }
        previous.insert(pid, cc);
    }
    (sync_errors, cc_errors)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let args = parse_args();
    let config = ConnectionConfig {
        server_addr: args.addr.clone(),
        tuner_path: args.tuner.clone(),
        connect_timeout: Duration::from_secs(5),
        read_timeout: Duration::from_secs(10),
        client_priority: args.priority,
        client_exclusive: args.exclusive,
        stream_class: if args.record {
            StreamClass::Record
        } else {
            StreamClass::View
        },
        ..ConnectionConfig::default()
    };
    let connection = Connection::new(config);
    if !connection.connect() || !connection.open_tuner() {
        return Err(io::Error::new(io::ErrorKind::ConnectionRefused, "BNDP open failed").into());
    }
    if !connection.set_channel_space(args.space, args.channel, args.priority, args.exclusive) {
        connection.close_tuner();
        return Err(io::Error::new(io::ErrorKind::Other, "SetChannelSpace failed").into());
    }
    if !connection.start_stream() {
        connection.close_tuner();
        return Err(io::Error::new(io::ErrorKind::Other, "StartStream failed").into());
    }

    let mut output = args.out.as_ref().map(File::create).transpose()?;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.seconds);
    let mut previous = HashMap::new();
    let mut received = 0u64;
    let mut sync_errors = 0u64;
    let mut cc_errors = 0u64;
    let mut scratch = vec![0u8; 1024 * 1024];

    while Instant::now() < deadline {
        let _ = connection.buffer().wait_data(Duration::from_millis(250));
        let (slice, _) = connection.buffer().read(scratch.len());
        if slice.is_empty() {
            continue;
        }
        let len = slice.len();
        scratch[..len].copy_from_slice(slice);
        connection.buffer().consume(len);
        received += len as u64;
        let (sync, cc) = continuity_errors(&scratch[..len], &mut previous);
        sync_errors += sync;
        cc_errors += cc;
        if let Some(file) = output.as_mut() {
            file.write_all(&scratch[..len])?;
        }
    }

    connection.stop_stream();
    connection.close_tuner();
    let elapsed = started.elapsed().as_secs_f64().max(f64::EPSILON);
    let mbps = received as f64 * 8.0 / 1_000_000.0 / elapsed;
    println!(
        "received_bytes={} average_mbps={:.3} sync_errors={} cc_errors={}",
        received, mbps, sync_errors, cc_errors
    );
    Ok(())
}

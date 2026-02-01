use clap::{ArgGroup, Parser, Subcommand};
use clap_num::maybe_hex;

use crate::tuner::Voltage;

#[derive(Debug, Parser)]
#[clap(name = "recisdb")]
#[clap(about = "recisdb can read both Unix chardev-based and BonDriver-based TV sources. ", long_about = None)]
#[clap(author = "maleicacid")]
#[clap(version)]
pub(crate) struct Cli {
    #[clap(subcommand)]
    pub command: Commands,
}

/// Output format for channel listing.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
pub(crate) enum OutputFormat {
    /// Human-readable table format
    #[default]
    Table,
    /// JSON format
    Json,
    /// CSV format
    Csv,
}

/// Broadcast type filter.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum BroadcastType {
    /// Terrestrial (ISDB-T)
    Terrestrial,
    /// BS (Broadcasting Satellite)
    Bs,
    /// CS (Communication Satellite)
    Cs,
    /// All types
    All,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Signal test.{n}
    /// This subcommand tests the signal quality of the tuner
    /// and prints the S/N rate in dB.{n}
    /// The signal quality is measured by the tuner's internal
    /// signal detector.
    #[clap(name = "checksignal")]
    Checksignal {
        /// The device name.{n}
        /// This is the name of the device as specified in the
        /// `/dev/` directory.{n}
        /// To use this option, you must specify the `-c` option.{n}
        /// When the device is a BonDriver-based device,
        /// the name of the dll comes here.{n}
        /// When the device is a Unix chardev-based device,
        /// the canonical path of the device comes here.{n}
        /// If the device has a V4L-DVB interface, there are 2 ways to point the frontend.{n}
        /// 1. (full) `-c /dev/dvb/adapter2/frontend0`{n}
        /// 2. (abbr.) `-c "2|0"`
        #[clap(short, long, value_name = "CANONICAL_PATH", required = true)]
        device: String,

        /// The channel name.{n}
        /// The channel name is a string that is defined in the
        /// `channels` module.
        #[clap(short, long, required = true)]
        channel: Option<String>,

        /// LNB voltage.
        /// If none, the LNB voltage is assumed unset.{n}
        #[clap(value_enum, long = "lnb")]
        lnb: Option<Voltage>,
    },
    /// Tune to a channel.
    /// This subcommand tunes the tuner to a channel and start recording.{n}
    /// The channel is specified by a channel name.{n}
    /// The recording directory is passed as an argument.
    // key0 and key1 are optional, but if they are specified, they must be specified together
    #[clap(group(
    ArgGroup::new("key")
    .args(& ["key0", "key1"])
    .requires_all(& ["key0", "key1"])
    .multiple(true)
    ))]
    Tune {
        /// The device name.{n}
        /// This is the name of the device as specified in the
        /// `/dev/` directory.{n}
        /// To use this option, you must specify the `-c` option.{n}
        /// When the device is a BonDriver-based device,
        /// the name of the DLL comes here.{n}
        /// When the device is a Unix chardev-based device,
        /// the canonical path of the device comes here.{n}
        /// If the device has a V4L-DVB interface, there are 2 ways to point the frontend.{n}
        /// 1. (full) `-c /dev/dvb/adapter2/frontend0`{n}
        /// 2. (abbr.) `-c "2|0"`
        #[clap(short = 'i', long, value_name = "CANONICAL_PATH", required = true)]
        device: Option<String>,

        /// The channel name.{n}
        /// The channel name is a string that is defined in the
        /// `channels` module.
        #[clap(short, long, required = true)]
        channel: Option<String>,

        /// The card reader name.
        #[clap(long)]
        card: Option<String>,

        /// Override the transport stream ID(TSID) to obtain the stream (especially in ISDB-S w/ V4L-DVB).
        #[clap(long, value_parser=maybe_hex::<u32>)]
        tsid: Option<u32>,

        /// The duration of the recording.{n}
        /// The duration of the recording is specified in seconds.
        /// If the duration is not specified, the recording will
        /// continue until the user stops it.{n}
        /// The duration is specified as a floating point number.{n}
        /// If the duration is 0.0, the recording will continue
        /// until the user stops it.
        /// If the duration is negative, the recording will
        /// continue until the user stops it.
        /// If the duration is positive, the recording will
        /// continue until the duration is over.
        #[clap(short, long, value_name = "seconds")]
        time: Option<f64>,

        /// Exit if the decoding fails while processing.
        #[clap(short = 'e', long)]
        exit_on_card_error: bool,

        /// Disable ARIB STD-B25 decoding.{n}
        /// If this flag is specified, ARIB STD-B25 decoding is not performed.
        #[clap(long = "no-decode")]
        no_decode: bool,
        /// Disable SIMD in MULTI2 processing.
        #[clap(long = "no-simd")]
        no_simd: bool,
        /// Disable null packet stripping.{n}
        /// If this flag is specified, the decoder won't discard meaningless packets automatically.
        #[clap(long = "no-strip")]
        no_strip: bool,

        /// LNB voltage.
        /// If none, the LNB voltage is assumed unset.{n}
        #[clap(value_enum, long = "lnb")]
        lnb: Option<Voltage>,

        /// The first working key (only available w/ "crypto" feature).{n}
        /// The first working key is a 64-bit hexadecimal number.{n}
        /// If the first working key is not specified, this subcommand
        /// will not decode ECM.
        #[clap(long = "key0")]
        key0: Option<Vec<String>>,
        /// The second working key (only available w/ "crypto" feature).{n}
        /// The second working key is a 64-bit hexadecimal number.{n}
        /// If the second working key is not specified, this subcommand
        /// will not decode ECM.
        #[clap(long = "key1")]
        key1: Option<Vec<String>>,

        /// The location of the output.{n}
        /// The location is a string that is specified as an
        /// absolute path.{n}
        /// If '-' is specified, the recording will be redirected to
        /// stdout.{n}
        /// If the specified file is a directory, this subcommand
        /// will stop.
        #[clap(required = true)]
        output: Option<String>,
    },
    /// Perform ARIB STD-B25 decoding on TS stream.
    #[clap(group(
    ArgGroup::new("key")
    .args(& ["key0", "key1"])
    .requires_all(& ["key0", "key1"])
    .multiple(true)
    ))]
    Decode {
        /// The source file name.{n}
        /// The source file name is a string that is specified as a
        /// file name.{n}
        /// If '--device' is specified, this parameter is ignored.
        #[clap(short = 'i', long = "input", value_name = "file", required = true)]
        source: Option<String>,

        /// Disable SIMD in MULTI2 processing.
        #[clap(long = "no-simd")]
        no_simd: bool,
        /// Disable null packet stripping.{n}
        /// If this flag is specified, the decoder won't discard meaningless packets automatically.
        #[clap(long = "no-strip")]
        no_strip: bool,

        /// The card reader name.
        #[clap(long)]
        card: Option<String>,

        /// The first working key (only available w/ "crypto" feature).{n}
        /// The first working key is a 64-bit hexadecimal number.{n}
        /// If the first working key is not specified, this subcommand
        /// will not decode ECM.
        #[clap(long = "key0")]
        key0: Option<Vec<String>>,
        /// The second working key (only available w/ "crypto" feature).{n}
        /// The second working key is a 64-bit hexadecimal number.{n}
        /// If the second working key is not specified, this subcommand
        /// will not decode ECM.
        #[clap(long = "key1")]
        key1: Option<Vec<String>>,

        /// The location of the output.{n}
        /// The location is a string that is specified as an
        /// absolute path.{n}
        /// If '-' is specified, the recording will be redirected to
        /// stdout.{n}
        /// If the specified file is a directory, this subcommand
        /// will stop.
        #[clap(required = true)]
        output: Option<String>,
    },
    #[cfg(windows)]
    Enumerate {
        #[clap(short = 'i', long, value_name = "CANONICAL_PATH", required = true)]
        device: String,
        #[clap(short, long, required = true)]
        space: u32,
    },

    /// Scan channels and store results in the database.{n}
    /// This subcommand tunes through all available channels and
    /// extracts NID/TSID/SID and service names.
    #[cfg(feature = "database")]
    #[clap(name = "scan")]
    Scan {
        /// The device name (BonDriver DLL path or chardev path).
        #[clap(short = 'i', long, value_name = "DEVICE_PATH", required = true)]
        device: String,

        /// Physical channels to scan (e.g., "13-62" for terrestrial).{n}
        /// If not specified, scans all known channels for the device type.
        #[clap(short, long)]
        range: Option<String>,

        /// Broadcast type to scan.
        #[clap(value_enum, long, default_value = "all")]
        broadcast_type: BroadcastType,

        /// Database file path.{n}
        /// If not specified, uses default location.
        #[clap(long, value_name = "DB_PATH")]
        database: Option<String>,

        /// Timeout per channel in seconds.
        #[clap(long, default_value = "5")]
        timeout: u32,

        /// LNB voltage (for satellite).
        #[clap(value_enum, long = "lnb")]
        lnb: Option<Voltage>,

        /// Continue scanning even if some channels fail.
        #[clap(long)]
        continue_on_error: bool,

        /// Show progress during scan.
        #[clap(long, short = 'v')]
        verbose: bool,
    },

    /// Show channel list from the database.{n}
    /// Displays stored channel information in various formats.
    #[cfg(feature = "database")]
    #[clap(name = "show")]
    Show {
        /// Database file path.
        #[clap(long, value_name = "DB_PATH")]
        database: Option<String>,

        /// Output format.
        #[clap(value_enum, long, short = 'f', default_value = "table")]
        format: OutputFormat,

        /// Filter by broadcast type.
        #[clap(value_enum, long, short = 't')]
        broadcast_type: Option<BroadcastType>,

        /// Filter by network ID.
        #[clap(long)]
        nid: Option<u16>,

        /// Filter by transport stream ID.
        #[clap(long)]
        tsid: Option<u16>,

        /// Show only enabled channels.
        #[clap(long)]
        enabled_only: bool,

        /// Sort by field (name, nid, sid, physical_ch).
        #[clap(long, default_value = "physical_ch")]
        sort: String,
    },

    /// Query channel information from the database.{n}
    /// Find channels by various criteria and print details.
    #[cfg(feature = "database")]
    #[clap(name = "query")]
    Query {
        /// Database file path.
        #[clap(long, value_name = "DB_PATH")]
        database: Option<String>,

        /// Search by channel name (partial match).
        #[clap(long, short = 'n')]
        name: Option<String>,

        /// Search by service ID.
        #[clap(long)]
        sid: Option<u16>,

        /// Search by network ID.
        #[clap(long)]
        nid: Option<u16>,

        /// Search by transport stream ID.
        #[clap(long)]
        tsid: Option<u16>,

        /// Search by remote control key ID.
        #[clap(long)]
        remote_key: Option<u8>,

        /// Output format.
        #[clap(value_enum, long, short = 'f', default_value = "table")]
        format: OutputFormat,

        /// Show detailed information.
        #[clap(long, short = 'd')]
        detail: bool,
    },

    /// Manage BonDrivers in the database.
    #[cfg(feature = "database")]
    #[clap(name = "driver")]
    Driver {
        #[clap(subcommand)]
        action: DriverAction,
    },
}

/// BonDriver management actions.
#[cfg(feature = "database")]
#[derive(Debug, Subcommand)]
pub(crate) enum DriverAction {
    /// Register a new BonDriver.
    Add {
        /// BonDriver DLL path.
        #[clap(required = true)]
        path: String,

        /// Display name for the driver.
        #[clap(long)]
        name: Option<String>,

        /// Database file path.
        #[clap(long, value_name = "DB_PATH")]
        database: Option<String>,
    },

    /// List registered BonDrivers.
    List {
        /// Database file path.
        #[clap(long, value_name = "DB_PATH")]
        database: Option<String>,

        /// Output format.
        #[clap(value_enum, long, short = 'f', default_value = "table")]
        format: OutputFormat,
    },

    /// Remove a BonDriver (and its channels).
    Remove {
        /// BonDriver ID or path.
        #[clap(required = true)]
        id_or_path: String,

        /// Database file path.
        #[clap(long, value_name = "DB_PATH")]
        database: Option<String>,

        /// Skip confirmation.
        #[clap(long, short = 'y')]
        yes: bool,
    },
}

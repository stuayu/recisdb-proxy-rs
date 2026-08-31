//! EIT (Event Information Table) parsing (ARIB STD-B10 §5.2.4 / ETSI EN 300 468).
//!
//! Covers the common MPEG-TS EIT table IDs. Terrestrial reception must use
//! `super::table_id::is_terrestrial_eit_table_id`: TR-B14 Vol. 4 §13.1
//! (printed p. 64) says other-TS EIT is not transmitted. The generic parser
//! still accepts other-TS IDs because the same crate also handles satellite
//! multiplexes, where they are valid.

use super::descriptor_tag;
use super::descriptors::{find_descriptor, parse_descriptor_loop};
use super::psi::PsiSection;
use crate::aribb24::decode_arib_string;

/// One event (programme) entry from an EIT event loop.
#[derive(Debug, Clone, Default)]
pub struct EitEvent {
    /// Event ID (unique per service).
    pub event_id: u16,
    /// Start time, epoch seconds (UTC). The on-wire MJD+BCD start_time is
    /// JST wall-clock; this has already been converted (JST = UTC+9).
    pub start_at: i64,
    /// Duration in seconds.
    pub duration_secs: u32,
    /// Running status (0-7, see ARIB STD-B10 table 5-5).
    pub running_status: u8,
    /// Free CA mode (`true` = scrambled).
    pub free_ca_mode: bool,
    /// Event name (short_event_descriptor).
    pub name: String,
    /// Short description (short_event_descriptor).
    pub description: String,
    /// Extended description: all extended_event_descriptor items and text,
    /// concatenated in descriptor_number order (a long description can be
    /// split across several descriptors).
    pub extended: String,
    /// `(content_nibble_level_1 << 4) | content_nibble_level_2` of the
    /// first content_descriptor genre entry, if present.
    pub genre: Option<u8>,
}

/// Parsed EIT section.
#[derive(Debug, Clone, Default)]
pub struct EitTable {
    /// Table ID (identifies present/following vs schedule, actual vs other).
    pub table_id: u8,
    /// Service ID (= PSI header's table_id_extension for EIT).
    pub service_id: u16,
    /// Transport stream ID.
    pub transport_stream_id: u16,
    /// Original network ID.
    pub original_network_id: u16,
    /// Version number.
    pub version_number: u8,
    /// Section number.
    pub section_number: u8,
    /// Last section number.
    pub last_section_number: u8,
    /// Segment_last_section_number (EIT-specific, schedule reassembly).
    pub segment_last_section_number: u8,
    /// Last table ID (EIT-specific, schedule reassembly).
    pub last_table_id: u8,
    /// Events carried by this section. Events whose start_time is
    /// undefined (all-1s on the wire) are skipped entirely.
    pub events: Vec<EitEvent>,
}

impl EitTable {
    /// Parse an EIT from a PSI section.
    pub fn parse(section: &PsiSection) -> Result<Self, &'static str> {
        if !super::table_id::is_eit_table_id(section.header.table_id) {
            return Err("Not an EIT section");
        }
        if !section.header.section_syntax_indicator {
            return Err("EIT section_syntax_indicator must be 1");
        }

        let data = section.data;
        if data.len() < 6 {
            return Err("EIT data too short");
        }

        let transport_stream_id = ((data[0] as u16) << 8) | data[1] as u16;
        let original_network_id = ((data[2] as u16) << 8) | data[3] as u16;
        let segment_last_section_number = data[4];
        let last_table_id = data[5];

        let mut events = Vec::new();
        let mut offset = 6usize;

        // Fixed part of each event: event_id(2) + start_time(5) + duration(3)
        // + running_status/free_CA/descriptors_loop_length(2) = 12 bytes.
        while offset + 12 <= data.len() {
            let event_id = ((data[offset] as u16) << 8) | data[offset + 1] as u16;
            let start_time_bytes = &data[offset + 2..offset + 7];
            let duration_bytes = &data[offset + 7..offset + 10];
            let running_status = (data[offset + 10] >> 5) & 0x07;
            let free_ca_mode = data[offset + 10] & 0x10 != 0;
            let descriptors_loop_length =
                (((data[offset + 10] as usize) & 0x0F) << 8) | data[offset + 11] as usize;

            offset += 12;

            if offset + descriptors_loop_length > data.len() {
                break;
            }
            let descriptors = &data[offset..offset + descriptors_loop_length];
            offset += descriptors_loop_length;

            // start_time is undefined when all 5 bytes are 1 — skip the
            // event rather than reporting a bogus 1858-era timestamp.
            let Some(start_at) = parse_start_time(start_time_bytes) else {
                continue;
            };
            let duration_secs = parse_duration(duration_bytes);
            let (name, description) = parse_short_event(descriptors);
            let extended = parse_extended_event(descriptors);
            let genre = parse_genre(descriptors);

            events.push(EitEvent {
                event_id,
                start_at,
                duration_secs,
                running_status,
                free_ca_mode,
                name,
                description,
                extended,
                genre,
            });
        }

        Ok(EitTable {
            table_id: section.header.table_id,
            service_id: section.header.table_id_extension,
            transport_stream_id,
            original_network_id,
            version_number: section.header.version_number,
            section_number: section.header.section_number,
            last_section_number: section.header.last_section_number,
            segment_last_section_number,
            last_table_id,
            events,
        })
    }
}

/// Convert a Modified Julian Date to (year, month, day), per ETSI EN 300 468
/// Annex C. Valid for the MJD range that ARIB/DVB broadcasts actually use
/// (roughly 1900-2100).
fn mjd_to_ymd(mjd: u32) -> (i32, u32, u32) {
    let mjd_f = mjd as f64;
    let y1 = ((mjd_f - 15078.2) / 365.25) as i64;
    let y1_days = (y1 as f64 * 365.25) as i64;
    let m1 = ((mjd_f - 14956.1 - y1_days as f64) / 30.6001) as i64;
    let m1_days = (m1 as f64 * 30.6001) as i64;
    let day = mjd as i64 - 14956 - y1_days - m1_days;
    let k: i64 = if m1 == 14 || m1 == 15 { 1 } else { 0 };
    let year = y1 + k + 1900;
    let month = m1 - 1 - k * 12;
    (year as i32, month as u32, day as u32)
}

/// Decode a 2-digit BCD byte (e.g. hour/minute/second) to its integer value.
fn bcd_byte_to_u32(b: u8) -> Option<u32> {
    let high = b >> 4;
    let low = b & 0x0F;
    if high > 9 || low > 9 {
        return None;
    }
    Some((high as u32) * 10 + low as u32)
}

/// Parse the 5-byte MJD+BCD `start_time` field. Returns `None` when the
/// field is the ARIB/DVB "undefined" sentinel (all bits 1) or when the
/// decoded date/time is not a valid calendar date/time.
fn parse_start_time(b: &[u8]) -> Option<i64> {
    if b.iter().all(|&x| x == 0xFF) {
        return None;
    }
    let mjd = ((b[0] as u32) << 8) | b[1] as u32;
    let (year, month, day) = mjd_to_ymd(mjd);
    let hour = bcd_byte_to_u32(b[2])?;
    let minute = bcd_byte_to_u32(b[3])?;
    let second = bcd_byte_to_u32(b[4])?;

    let date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    let time = chrono::NaiveTime::from_hms_opt(hour, minute, second)?;
    let naive = chrono::NaiveDateTime::new(date, time);

    // `naive` is a JST wall-clock reading; JST = UTC+9, so subtract 9h to
    // get the UTC epoch seconds we store.
    Some(naive.and_utc().timestamp() - 9 * 3600)
}

/// Parse the 3-byte BCD `duration` field into total seconds.
fn parse_duration(b: &[u8]) -> u32 {
    if b == [0xFF, 0xFF, 0xFF] {
        return 0;
    }
    let Some(h) = bcd_byte_to_u32(b[0]) else { return 0 };
    let Some(m) = bcd_byte_to_u32(b[1]) else { return 0 };
    let Some(s) = bcd_byte_to_u32(b[2]) else { return 0 };
    if m >= 60 || s >= 60 {
        return 0;
    }
    h * 3600 + m * 60 + s
}

/// Extract (event_name, text) from the short_event_descriptor (0x4D), if
/// present. ARIB STD-B10 mandates exactly one per event.
fn parse_short_event(descriptors: &[u8]) -> (String, String) {
    let Some(data) = find_descriptor(descriptors, descriptor_tag::SHORT_EVENT) else {
        return (String::new(), String::new());
    };
    // ISO_639_language_code(3) + event_name_length(1)
    if data.len() < 4 {
        return (String::new(), String::new());
    }
    let name_len = data[3] as usize;
    if data.len() < 4 + name_len + 1 {
        return (String::new(), String::new());
    }
    let name = decode_arib_string(&data[4..4 + name_len]);

    let text_len_offset = 4 + name_len;
    let text_len = data[text_len_offset] as usize;
    if data.len() < text_len_offset + 1 + text_len {
        return (name, String::new());
    }
    let text = decode_arib_string(&data[text_len_offset + 1..text_len_offset + 1 + text_len]);
    (name, text)
}

/// Concatenate every extended_event_descriptor (0x4E) attached to an event,
/// in descriptor_number order, each rendered as its item pairs followed by
/// its trailing text. A single long description is split by the broadcaster
/// across several of these descriptors (`descriptor_number`/
/// `last_descriptor_number`), so they must be joined in order to recover it.
fn parse_extended_event(descriptors: &[u8]) -> String {
    let mut parts: Vec<(u8, String)> = Vec::new();

    for (tag, data) in parse_descriptor_loop(descriptors) {
        if tag != descriptor_tag::EXTENDED_EVENT {
            continue;
        }
        // descriptor_number(4)+last_descriptor_number(4) [0] +
        // ISO_639_language_code(3) [1..4] + length_of_items(1) [4]
        if data.len() < 5 {
            continue;
        }
        let descriptor_number = (data[0] >> 4) & 0x0F;
        let items_length = data[4] as usize;
        if data.len() < 5 + items_length {
            continue;
        }
        let items = &data[5..5 + items_length];

        // Decode all item descriptions/values and trailing text as one
        // 8-bit-code stream. An ESC designation may legally occur only in
        // the first item; decoding each field independently would reset
        // GL/GR and corrupt the following fields.
        let mut fields: Vec<(bool, Vec<u8>)> = Vec::new();
        let mut off = 0usize;
        while off < items.len() {
            let item_desc_len = items[off] as usize;
            off += 1;
            if off + item_desc_len > items.len() {
                break;
            }
            if item_desc_len != 0 {
                fields.push((true, items[off..off + item_desc_len].to_vec()));
            }
            off += item_desc_len;

            if off >= items.len() {
                break;
            }
            let item_len = items[off] as usize;
            off += 1;
            if off + item_len > items.len() {
                break;
            }
            if item_len != 0 {
                fields.push((false, items[off..off + item_len].to_vec()));
            }
            off += item_len;
        }

        let text_offset = 5 + items_length;
        if text_offset < data.len() {
            let text_len = data[text_offset] as usize;
            if data.len() >= text_offset + 1 + text_len {
                if text_len != 0 {
                    fields.push((false, data[text_offset + 1..text_offset + 1 + text_len].to_vec()));
                }
            }
        }

        let mut encoded = Vec::new();
        for (index, (_, field)) in fields.iter().enumerate() {
            if index != 0 {
                encoded.push(0x0D);
            }
            encoded.extend_from_slice(field);
        }
        let decoded = decode_arib_string(&encoded);
        let mut buf = String::new();
        let mut rendered = decoded.split('\n');
        let mut pending_description: Option<&str> = None;
        for (is_description, _) in &fields {
            let value = rendered.next().unwrap_or("");
            if *is_description {
                pending_description = Some(value);
            } else {
                if let Some(description) = pending_description.take() {
                    if !description.is_empty() {
                        buf.push_str(description);
                        buf.push_str(": ");
                    }
                }
                buf.push_str(value);
                buf.push('\n');
            }
        }
        parts.push((descriptor_number, buf));
    }

    parts.sort_by_key(|(n, _)| *n);
    parts.into_iter().map(|(_, s)| s).collect::<Vec<_>>().join("")
}

/// Extract the genre byte from the first content_descriptor (0x54) entry,
/// if present: `(content_nibble_level_1 << 4) | content_nibble_level_2`.
fn parse_genre(descriptors: &[u8]) -> Option<u8> {
    let data = find_descriptor(descriptors, descriptor_tag::CONTENT)?;
    data.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts_analyzer::psi::PsiHeader;
    use crate::ts_analyzer::table_id;

    const ARIB_ALNUM_G0: [u8; 3] = [0x1b, 0x28, 0x4a];

    #[test]
    fn test_mjd_to_ymd_known_dates() {
        // MJD 40587 = 1970-01-01 (Unix epoch day), a widely-cited reference
        // value for verifying the ETSI EN 300 468 Annex C formula.
        assert_eq!(mjd_to_ymd(40587), (1970, 1, 1));
        // MJD 51544 = 2000-01-01 (another commonly cited reference point).
        assert_eq!(mjd_to_ymd(51544), (2000, 1, 1));
        // MJD 60310 = 2024-01-01 (51544 + 8766 days: 24 years incl. 6 leap
        // years 2000/04/08/12/16/20 between the two reference points).
        assert_eq!(mjd_to_ymd(60310), (2024, 1, 1));
    }

    #[test]
    fn test_bcd_byte_to_u32() {
        assert_eq!(bcd_byte_to_u32(0x00), Some(0));
        assert_eq!(bcd_byte_to_u32(0x09), Some(9));
        assert_eq!(bcd_byte_to_u32(0x23), Some(23));
        assert_eq!(bcd_byte_to_u32(0x59), Some(59));
    }

    #[test]
    fn test_parse_start_time_jst_to_utc_epoch() {
        // MJD 60310 (2024-01-01) at 00:00:00 JST = 2023-12-31 15:00:00 UTC.
        let mjd_bytes = (60310u16).to_be_bytes();
        let bytes = [mjd_bytes[0], mjd_bytes[1], 0x00, 0x00, 0x00];
        let epoch = parse_start_time(&bytes).unwrap();

        let expected = chrono::NaiveDate::from_ymd_opt(2023, 12, 31)
            .unwrap()
            .and_hms_opt(15, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        assert_eq!(epoch, expected);
    }

    #[test]
    fn test_parse_start_time_undefined_is_none() {
        assert_eq!(parse_start_time(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF]), None);
    }

    #[test]
    fn test_parse_duration() {
        // 1h23m45s
        assert_eq!(parse_duration(&[0x01, 0x23, 0x45]), 3600 + 23 * 60 + 45);
        assert_eq!(parse_duration(&[0x00, 0x00, 0x00]), 0);
        assert_eq!(parse_duration(&[0xFF, 0xFF, 0xFF]), 0);
        assert_eq!(parse_duration(&[0x00, 0x60, 0x00]), 0);
    }

    fn arib(s: &[u8]) -> Vec<u8> {
        let mut v = ARIB_ALNUM_G0.to_vec();
        v.extend_from_slice(s);
        v
    }

    #[test]
    fn test_parse_short_event() {
        let name = arib(b"NEWS");
        let text = arib(b"Today");
        let mut data = vec![0x00, 0x00, 0x00]; // ISO_639 "jpn"-ish placeholder
        data.push(name.len() as u8);
        data.extend_from_slice(&name);
        data.push(text.len() as u8);
        data.extend_from_slice(&text);

        let mut descriptors = vec![descriptor_tag::SHORT_EVENT, data.len() as u8];
        descriptors.extend_from_slice(&data);

        let (n, d) = parse_short_event(&descriptors);
        assert_eq!(n, "ＮＥＷＳ");
        assert_eq!(d, "Ｔｏｄａｙ");
    }

    #[test]
    fn test_parse_genre() {
        let data = vec![0x21, 0x00]; // level1=2, level2=1
        let mut descriptors = vec![descriptor_tag::CONTENT, data.len() as u8];
        descriptors.extend_from_slice(&data);

        assert_eq!(parse_genre(&descriptors), Some(0x21));
        assert_eq!(parse_genre(&[]), None);
    }

    #[test]
    fn extended_event_keeps_code_set_across_item_fields() {
        // The second item has no repeated ESC designation. It must still be
        // decoded as alphanumeric because the 8-bit-code state is continuous
        // across the descriptor's item loop (STD-B24 Vol. 1 Part 3 §7.1.1).
        let first = arib(b"A");
        let second = b"B";
        let mut data = vec![0x00, 0x00, 0x00, 0x00, 0x07]; // number + "jpn" + items length
        data.extend_from_slice(&[0x00, first.len() as u8]);
        data.extend_from_slice(&first);
        data.push(0x00);
        data.push(second.len() as u8);
        data.extend_from_slice(second);
        data.push(0x00); // empty trailing text

        let mut descriptors = vec![descriptor_tag::EXTENDED_EVENT, data.len() as u8];
        descriptors.extend_from_slice(&data);
        assert_eq!(parse_extended_event(&descriptors), "Ａ\nＢ\n");
    }

    #[test]
    fn test_parse_eit_event_loop() {
        // Build one EIT section by hand: 1 event with a short_event
        // descriptor, MJD=60310 (2024-01-01) 00:00:00 JST, duration 30m.
        let short_event_payload = {
            let name = arib(b"CH");
            let text = arib(b"Desc");
            let mut d = vec![0x00, 0x00, 0x00];
            d.push(name.len() as u8);
            d.extend_from_slice(&name);
            d.push(text.len() as u8);
            d.extend_from_slice(&text);
            d
        };
        let mut descriptors = vec![descriptor_tag::SHORT_EVENT, short_event_payload.len() as u8];
        descriptors.extend_from_slice(&short_event_payload);

        let mjd_bytes = (60310u16).to_be_bytes();
        let mut data = vec![
            0x7F, 0xE1, // transport_stream_id
            0x00, 0x04, // original_network_id
            0x00, // segment_last_section_number
            0x4E, // last_table_id
        ];
        // event_id
        data.extend_from_slice(&[0x00, 0x01]);
        // start_time (MJD + BCD 00:00:00)
        data.extend_from_slice(&[mjd_bytes[0], mjd_bytes[1], 0x00, 0x00, 0x00]);
        // duration 00:30:00
        data.extend_from_slice(&[0x00, 0x30, 0x00]);
        // running_status=4, free_CA=0, descriptors_loop_length
        let dll = descriptors.len() as u16;
        data.push((4 << 5) | (((dll >> 8) & 0x0F) as u8));
        data.push((dll & 0xFF) as u8);
        data.extend_from_slice(&descriptors);

        let header = PsiHeader {
            table_id: table_id::EIT_PF_ACTUAL,
            section_syntax_indicator: true,
            section_length: (5 + data.len()) as u16,
            table_id_extension: 0x0408, // service_id
            version_number: 1,
            current_next_indicator: true,
            section_number: 0,
            last_section_number: 0,
        };
        let section = PsiSection { header, data: &data, crc32: 0 };

        let eit = EitTable::parse(&section).unwrap();
        assert_eq!(eit.service_id, 0x0408);
        assert_eq!(eit.transport_stream_id, 0x7FE1);
        assert_eq!(eit.original_network_id, 0x0004);
        assert_eq!(eit.events.len(), 1);

        let ev = &eit.events[0];
        assert_eq!(ev.event_id, 1);
        assert_eq!(ev.duration_secs, 30 * 60);
        assert_eq!(ev.running_status, 4);
        assert!(!ev.free_ca_mode);
        assert_eq!(ev.name, "ＣＨ");
        assert_eq!(ev.description, "Ｄｅｓｃ");
    }

    #[test]
    fn test_parse_eit_rejects_non_eit_table_id() {
        let header = PsiHeader {
            table_id: table_id::SDT_ACTUAL,
            section_syntax_indicator: true,
            section_length: 10,
            table_id_extension: 0,
            version_number: 0,
            current_next_indicator: true,
            section_number: 0,
            last_section_number: 0,
        };
        let data = [0u8; 8];
        let section = PsiSection { header, data: &data, crc32: 0 };
        assert!(EitTable::parse(&section).is_err());
    }
}

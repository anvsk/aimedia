//! Clean-room MPEG-TS packet parsing, streaming demux/mux, and stream probing.

pub mod elementary;
pub mod stream;

pub use stream::{
    AUDIO_PID, DemuxEvent, ElementaryPacket as StreamPacket, MuxPacket, MuxStream, PAT_PID,
    PMT_PID, ProgramMap, StreamDemuxer, StreamMuxer, VIDEO_PID,
};

use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{self, Read},
    path::Path,
};

use serde::Serialize;
use thiserror::Error;

pub const PACKET_SIZE: usize = 188;
pub const SYNC_BYTE: u8 = 0x47;
pub const NULL_PID: u16 = 0x1fff;

#[derive(Debug, Error)]
pub enum TsError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("packet must contain exactly 188 bytes, got {0}")]
    InvalidPacketLength(usize),
    #[error("invalid TS sync byte 0x{0:02x}")]
    InvalidSyncByte(u8),
    #[error("adaptation_field_control value 0 is reserved")]
    ReservedAdaptationControl,
    #[error("adaptation field length {length} exceeds packet boundary")]
    AdaptationFieldOverflow { length: usize },
    #[error("PCR flag is set but the adaptation field is too short")]
    TruncatedPcr,
    #[error("could not find a stable 188-byte MPEG-TS sync pattern")]
    SyncNotFound,
    #[error("PSI section is truncated")]
    TruncatedSection,
    #[error("PSI section CRC is invalid")]
    InvalidSectionCrc,
    #[error("PES packet is malformed: {0}")]
    MalformedPes(&'static str),
    #[error("PES reassembly exceeded the {limit_bytes}-byte hard limit")]
    PesBufferOverflow { limit_bytes: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptationControl {
    PayloadOnly,
    AdaptationOnly,
    AdaptationAndPayload,
}

impl AdaptationControl {
    #[must_use]
    pub const fn has_payload(self) -> bool {
        matches!(self, Self::PayloadOnly | Self::AdaptationAndPayload)
    }

    #[must_use]
    pub const fn has_adaptation(self) -> bool {
        matches!(self, Self::AdaptationOnly | Self::AdaptationAndPayload)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TsHeader {
    pub transport_error: bool,
    pub payload_unit_start: bool,
    pub transport_priority: bool,
    pub pid: u16,
    pub scrambling_control: u8,
    pub adaptation_control: AdaptationControl,
    pub continuity_counter: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct AdaptationField {
    pub discontinuity: bool,
    pub random_access: bool,
    pub elementary_stream_priority: bool,
    pub pcr_27mhz: Option<u64>,
}

#[derive(Debug)]
pub struct TsPacket<'a> {
    pub header: TsHeader,
    pub adaptation: Option<AdaptationField>,
    pub payload: &'a [u8],
}

impl<'a> TsPacket<'a> {
    pub fn parse(packet: &'a [u8]) -> Result<Self, TsError> {
        if packet.len() != PACKET_SIZE {
            return Err(TsError::InvalidPacketLength(packet.len()));
        }
        if packet[0] != SYNC_BYTE {
            return Err(TsError::InvalidSyncByte(packet[0]));
        }

        let adaptation_control = match (packet[3] >> 4) & 0x03 {
            0 => return Err(TsError::ReservedAdaptationControl),
            1 => AdaptationControl::PayloadOnly,
            2 => AdaptationControl::AdaptationOnly,
            3 => AdaptationControl::AdaptationAndPayload,
            _ => unreachable!("masked to two bits"),
        };
        let header = TsHeader {
            transport_error: packet[1] & 0x80 != 0,
            payload_unit_start: packet[1] & 0x40 != 0,
            transport_priority: packet[1] & 0x20 != 0,
            pid: (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]),
            scrambling_control: (packet[3] >> 6) & 0x03,
            adaptation_control,
            continuity_counter: packet[3] & 0x0f,
        };

        let mut payload_start = 4;
        let adaptation = if adaptation_control.has_adaptation() {
            let length = usize::from(packet[4]);
            let end = 5_usize.saturating_add(length);
            if end > PACKET_SIZE {
                return Err(TsError::AdaptationFieldOverflow { length });
            }
            payload_start = end;
            if length == 0 {
                Some(AdaptationField {
                    discontinuity: false,
                    random_access: false,
                    elementary_stream_priority: false,
                    pcr_27mhz: None,
                })
            } else {
                let flags = packet[5];
                let pcr_27mhz = if flags & 0x10 != 0 {
                    if length < 7 {
                        return Err(TsError::TruncatedPcr);
                    }
                    Some(parse_pcr(&packet[6..12]))
                } else {
                    None
                };
                Some(AdaptationField {
                    discontinuity: flags & 0x80 != 0,
                    random_access: flags & 0x40 != 0,
                    elementary_stream_priority: flags & 0x20 != 0,
                    pcr_27mhz,
                })
            }
        } else {
            None
        };

        let payload = if adaptation_control.has_payload() {
            &packet[payload_start..]
        } else {
            &[]
        };
        Ok(Self {
            header,
            adaptation,
            payload,
        })
    }
}

fn parse_pcr(bytes: &[u8]) -> u64 {
    let base = (u64::from(bytes[0]) << 25)
        | (u64::from(bytes[1]) << 17)
        | (u64::from(bytes[2]) << 9)
        | (u64::from(bytes[3]) << 1)
        | (u64::from(bytes[4]) >> 7);
    let extension = (u64::from(bytes[4] & 0x01) << 8) | u64::from(bytes[5]);
    base * 300 + extension
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    pub packet_size: usize,
    pub sync_offset: usize,
    pub packets: u64,
    pub trailing_bytes: usize,
    pub programs: Vec<ProgramInfo>,
    pub pids: Vec<PidReport>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgramInfo {
    pub program_number: u16,
    pub pmt_pid: u16,
    pub pcr_pid: Option<u16>,
    pub streams: Vec<ElementaryStream>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementaryStream {
    pub pid: u16,
    pub stream_type: u8,
    pub codec: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PidReport {
    pub pid: u16,
    pub packets: u64,
    pub payload_packets: u64,
    pub continuity_errors: u64,
    pub transport_errors: u64,
    pub random_access_packets: u64,
    pub first_pcr_27mhz: Option<u64>,
    pub last_pcr_27mhz: Option<u64>,
    pub stream_type: Option<u8>,
    pub codec: Option<&'static str>,
}

#[derive(Debug, Default)]
struct MutablePidStats {
    packets: u64,
    payload_packets: u64,
    continuity_errors: u64,
    transport_errors: u64,
    random_access_packets: u64,
    first_pcr_27mhz: Option<u64>,
    last_pcr_27mhz: Option<u64>,
    stream_type: Option<u8>,
}

#[derive(Debug, Clone, Copy)]
struct ContinuityState {
    value: u8,
}

pub fn probe_path(path: impl AsRef<Path>) -> Result<ProbeReport, TsError> {
    let file = File::open(path)?;
    probe_reader(file)
}

pub fn probe_reader(mut reader: impl Read) -> Result<ProbeReport, TsError> {
    let mut data = Vec::new();
    reader.read_to_end(&mut data)?;
    probe_bytes(&data)
}

pub fn probe_bytes(data: &[u8]) -> Result<ProbeReport, TsError> {
    let sync_offset = find_sync_offset(data).ok_or(TsError::SyncNotFound)?;
    let aligned = &data[sync_offset..];
    let packet_count = aligned.len() / PACKET_SIZE;
    let trailing_bytes = aligned.len() % PACKET_SIZE;

    let mut pid_stats: BTreeMap<u16, MutablePidStats> = BTreeMap::new();
    let mut continuity: HashMap<u16, ContinuityState> = HashMap::new();
    let mut programs: BTreeMap<u16, ProgramInfo> = BTreeMap::new();
    let mut pmt_to_program: HashMap<u16, u16> = HashMap::new();

    for raw in aligned[..packet_count * PACKET_SIZE].chunks_exact(PACKET_SIZE) {
        let packet = TsPacket::parse(raw)?;
        let stats = pid_stats.entry(packet.header.pid).or_default();
        stats.packets += 1;
        if packet.header.transport_error {
            stats.transport_errors += 1;
        }
        if let Some(adaptation) = packet.adaptation {
            if adaptation.random_access {
                stats.random_access_packets += 1;
            }
            if let Some(pcr) = adaptation.pcr_27mhz {
                stats.first_pcr_27mhz.get_or_insert(pcr);
                stats.last_pcr_27mhz = Some(pcr);
            }
        }

        if packet.header.adaptation_control.has_payload() && packet.header.pid != NULL_PID {
            stats.payload_packets += 1;
            let discontinuity = packet
                .adaptation
                .is_some_and(|adaptation| adaptation.discontinuity);
            if let Some(previous) = continuity.get(&packet.header.pid) {
                let expected = (previous.value + 1) & 0x0f;
                if !discontinuity && packet.header.continuity_counter != expected {
                    stats.continuity_errors += 1;
                }
            }
            continuity.insert(
                packet.header.pid,
                ContinuityState {
                    value: packet.header.continuity_counter,
                },
            );
        }

        if packet.header.payload_unit_start && packet.header.pid == 0 {
            if let Ok(pat_entries) = parse_pat(packet.payload) {
                for (program_number, pmt_pid) in pat_entries {
                    pmt_to_program.insert(pmt_pid, program_number);
                    programs
                        .entry(program_number)
                        .and_modify(|program| program.pmt_pid = pmt_pid)
                        .or_insert(ProgramInfo {
                            program_number,
                            pmt_pid,
                            pcr_pid: None,
                            streams: Vec::new(),
                        });
                }
            }
        } else if packet.header.payload_unit_start {
            if let Some(program_number) = pmt_to_program.get(&packet.header.pid).copied() {
                if let Ok((pcr_pid, streams)) = parse_pmt(packet.payload) {
                    for stream in &streams {
                        pid_stats.entry(stream.pid).or_default().stream_type =
                            Some(stream.stream_type);
                    }
                    if let Some(program) = programs.get_mut(&program_number) {
                        program.pcr_pid = Some(pcr_pid);
                        program.streams = streams;
                    }
                }
            }
        }
    }

    let pids = pid_stats
        .into_iter()
        .map(|(pid, stats)| PidReport {
            pid,
            packets: stats.packets,
            payload_packets: stats.payload_packets,
            continuity_errors: stats.continuity_errors,
            transport_errors: stats.transport_errors,
            random_access_packets: stats.random_access_packets,
            first_pcr_27mhz: stats.first_pcr_27mhz,
            last_pcr_27mhz: stats.last_pcr_27mhz,
            stream_type: stats.stream_type,
            codec: stats.stream_type.map(codec_name),
        })
        .collect();

    Ok(ProbeReport {
        packet_size: PACKET_SIZE,
        sync_offset,
        packets: packet_count as u64,
        trailing_bytes,
        programs: programs.into_values().collect(),
        pids,
    })
}

fn find_sync_offset(data: &[u8]) -> Option<usize> {
    if data.len() < PACKET_SIZE {
        return None;
    }
    let checks = (data.len() / PACKET_SIZE).min(5);
    (0..PACKET_SIZE.min(data.len())).find(|offset| {
        (0..checks).all(|index| {
            offset
                .checked_add(index * PACKET_SIZE)
                .is_some_and(|position| data.get(position) == Some(&SYNC_BYTE))
        })
    })
}

fn psi_section(payload: &[u8], expected_table_id: u8) -> Result<&[u8], TsError> {
    let pointer = usize::from(*payload.first().ok_or(TsError::TruncatedSection)?);
    let start = 1_usize
        .checked_add(pointer)
        .ok_or(TsError::TruncatedSection)?;
    let section = payload.get(start..).ok_or(TsError::TruncatedSection)?;
    if section.len() < 3 || section[0] != expected_table_id {
        return Err(TsError::TruncatedSection);
    }
    let section_length = (usize::from(section[1] & 0x0f) << 8) | usize::from(section[2]);
    let total = 3_usize
        .checked_add(section_length)
        .ok_or(TsError::TruncatedSection)?;
    section.get(..total).ok_or(TsError::TruncatedSection)
}

fn parse_pat(payload: &[u8]) -> Result<Vec<(u16, u16)>, TsError> {
    let section = psi_section(payload, 0x00)?;
    if section.len() < 12 {
        return Err(TsError::TruncatedSection);
    }
    let mut entries = Vec::new();
    let entries_end = section.len().saturating_sub(4);
    let mut cursor = 8;
    while cursor + 4 <= entries_end {
        let program_number = u16::from_be_bytes([section[cursor], section[cursor + 1]]);
        let pid = (u16::from(section[cursor + 2] & 0x1f) << 8) | u16::from(section[cursor + 3]);
        if program_number != 0 {
            entries.push((program_number, pid));
        }
        cursor += 4;
    }
    Ok(entries)
}

fn parse_pmt(payload: &[u8]) -> Result<(u16, Vec<ElementaryStream>), TsError> {
    let section = psi_section(payload, 0x02)?;
    if section.len() < 16 {
        return Err(TsError::TruncatedSection);
    }
    let pcr_pid = (u16::from(section[8] & 0x1f) << 8) | u16::from(section[9]);
    let program_info_length = (usize::from(section[10] & 0x0f) << 8) | usize::from(section[11]);
    let mut cursor = 12_usize
        .checked_add(program_info_length)
        .ok_or(TsError::TruncatedSection)?;
    let streams_end = section.len().saturating_sub(4);
    if cursor > streams_end {
        return Err(TsError::TruncatedSection);
    }
    let mut streams = Vec::new();
    while cursor + 5 <= streams_end {
        let stream_type = section[cursor];
        let pid = (u16::from(section[cursor + 1] & 0x1f) << 8) | u16::from(section[cursor + 2]);
        let es_info_length =
            (usize::from(section[cursor + 3] & 0x0f) << 8) | usize::from(section[cursor + 4]);
        streams.push(ElementaryStream {
            pid,
            stream_type,
            codec: codec_name(stream_type),
        });
        cursor = cursor
            .checked_add(5 + es_info_length)
            .ok_or(TsError::TruncatedSection)?;
        if cursor > streams_end {
            return Err(TsError::TruncatedSection);
        }
    }
    Ok((pcr_pid, streams))
}

#[must_use]
pub const fn codec_name(stream_type: u8) -> &'static str {
    match stream_type {
        0x01 => "MPEG-1 Video",
        0x02 => "MPEG-2 Video",
        0x03 => "MPEG-1 Audio",
        0x04 => "MPEG-2 Audio",
        0x0f => "AAC ADTS",
        0x11 => "AAC LATM",
        0x1b => "H.264/AVC",
        0x24 => "H.265/HEVC",
        0x33 => "VVC",
        0x81 => "AC-3 (private)",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::{PACKET_SIZE, probe_bytes};

    #[test]
    fn reports_continuity_errors_without_panicking() {
        let mut packet = [0_u8; PACKET_SIZE];
        packet[0] = 0x47;
        packet[1] = 0x01;
        packet[2] = 0x00;
        packet[3] = 0x10;

        let mut stream = Vec::new();
        stream.extend_from_slice(&packet);
        stream.extend_from_slice(&packet);
        stream.extend_from_slice(&packet);

        let report = probe_bytes(&stream).expect("valid packet framing");
        assert_eq!(report.packets, 3);
        assert_eq!(report.pids.len(), 1);
        assert_eq!(report.pids[0].pid, 0x0100);
        assert_eq!(report.pids[0].continuity_errors, 2);
    }
}

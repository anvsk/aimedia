use std::collections::HashMap;

use bytes::Bytes;
use serde::Serialize;

use crate::{
    PACKET_SIZE, SYNC_BYTE, TsError, TsPacket,
    elementary::{parse_adts_stream, parse_annex_b},
};

pub const PAT_PID: u16 = 0x0000;
pub const PMT_PID: u16 = 0x1000;
pub const VIDEO_PID: u16 = 0x0100;
pub const AUDIO_PID: u16 = 0x0101;
const PTS_MODULUS: u64 = 1_u64 << 33;
const PTS_HALF_RANGE: u64 = PTS_MODULUS / 2;
const MAX_PES_BYTES: usize = 8 * 1_024 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxStream {
    Video,
    Audio,
}

#[derive(Debug, Clone)]
pub struct MuxPacket {
    pub stream: MuxStream,
    pub pts_90khz: u64,
    pub dts_90khz: Option<u64>,
    pub keyframe: bool,
    pub data: Bytes,
}

#[derive(Debug, Clone)]
pub struct ElementaryPacket {
    pub stream: MuxStream,
    pub pid: u16,
    pub pts_90khz: u64,
    pub dts_90khz: Option<u64>,
    pub keyframe: bool,
    pub discontinuity: bool,
    pub data: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgramMap {
    pub program_number: u16,
    pub pmt_pid: u16,
    pub pcr_pid: u16,
    pub video_pid: u16,
    pub audio_pid: u16,
    pub version: u8,
}

#[derive(Debug, Clone)]
pub enum DemuxEvent {
    ProgramMap(ProgramMap),
    Packet(ElementaryPacket),
    ContinuityError { pid: u16, expected: u8, actual: u8 },
    Discontinuity { pid: u16 },
    SyncRecovered { discarded_bytes: usize },
    CorruptData { pid: Option<u16>, reason: String },
}

#[derive(Debug, Default)]
struct SectionAssembler {
    data: Vec<u8>,
    expected: Option<usize>,
}

impl SectionAssembler {
    fn reset(&mut self) {
        self.data.clear();
        self.expected = None;
    }

    fn push(&mut self, payload: &[u8], start: bool) -> Vec<Vec<u8>> {
        let mut completed = Vec::new();
        let mut cursor = 0_usize;

        if start {
            let Some(pointer) = payload.first().copied().map(usize::from) else {
                return completed;
            };
            cursor = 1;
            let continuation_end = cursor.saturating_add(pointer).min(payload.len());
            if !self.data.is_empty() {
                self.append(&payload[cursor..continuation_end], &mut completed);
            }
            if !self.data.is_empty() {
                self.reset();
            }
            cursor = continuation_end;
        } else if self.data.is_empty() {
            return completed;
        }

        while cursor < payload.len() {
            if payload[cursor] == 0xff {
                break;
            }
            let before = cursor;
            cursor += self.append(&payload[cursor..], &mut completed);
            if cursor == before {
                break;
            }
        }
        completed
    }

    fn append(&mut self, input: &[u8], completed: &mut Vec<Vec<u8>>) -> usize {
        if input.is_empty() {
            return 0;
        }
        let needed_for_header = 3_usize.saturating_sub(self.data.len());
        let header_take = needed_for_header.min(input.len());
        self.data.extend_from_slice(&input[..header_take]);
        let mut consumed = header_take;
        if self.data.len() >= 3 && self.expected.is_none() {
            let section_length =
                (usize::from(self.data[1] & 0x0f) << 8) | usize::from(self.data[2]);
            self.expected = Some(3 + section_length);
        }

        if let Some(expected) = self.expected {
            let remaining = expected.saturating_sub(self.data.len());
            let take = remaining.min(input.len().saturating_sub(consumed));
            self.data
                .extend_from_slice(&input[consumed..consumed.saturating_add(take)]);
            consumed += take;
            if self.data.len() == expected {
                completed.push(std::mem::take(&mut self.data));
                self.expected = None;
            }
        }
        consumed
    }
}

#[derive(Debug, Default)]
struct PesAssembler {
    data: Vec<u8>,
    expected: Option<usize>,
}

impl PesAssembler {
    fn reset(&mut self) {
        self.data.clear();
        self.expected = None;
    }

    fn start(&mut self, payload: &[u8]) -> Result<Option<Vec<u8>>, TsError> {
        let previous = (!self.data.is_empty()).then(|| std::mem::take(&mut self.data));
        self.expected = None;
        if payload.len() > MAX_PES_BYTES {
            return Err(TsError::PesBufferOverflow {
                limit_bytes: MAX_PES_BYTES,
            });
        }
        self.data.extend_from_slice(payload);
        self.update_expected();
        Ok(previous)
    }

    fn push(&mut self, payload: &[u8]) -> Result<(), TsError> {
        if self.data.is_empty() {
            return Ok(());
        }
        if self.data.len().saturating_add(payload.len()) > MAX_PES_BYTES {
            self.reset();
            return Err(TsError::PesBufferOverflow {
                limit_bytes: MAX_PES_BYTES,
            });
        }
        self.data.extend_from_slice(payload);
        self.update_expected();
        Ok(())
    }

    fn update_expected(&mut self) {
        if self.data.len() >= 6 && self.expected.is_none() {
            let declared = usize::from(u16::from_be_bytes([self.data[4], self.data[5]]));
            if declared != 0 {
                self.expected = Some(6 + declared);
            }
        }
    }

    fn take_complete(&mut self) -> Option<Vec<u8>> {
        let expected = self.expected?;
        if self.data.len() < expected {
            return None;
        }
        let remainder = self.data.split_off(expected);
        let packet = std::mem::replace(&mut self.data, remainder);
        self.expected = None;
        Some(packet)
    }

    fn finish(&mut self) -> Option<Vec<u8>> {
        self.expected = None;
        (!self.data.is_empty()).then(|| std::mem::take(&mut self.data))
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct TimestampUnwrapper {
    previous_raw: Option<u64>,
    epoch: u64,
}

impl TimestampUnwrapper {
    fn unwrap(&mut self, raw: u64) -> u64 {
        let raw = raw & (PTS_MODULUS - 1);
        if let Some(previous) = self.previous_raw {
            if raw.saturating_add(PTS_HALF_RANGE) < previous {
                self.epoch = self.epoch.saturating_add(PTS_MODULUS);
            }
        }
        self.previous_raw = Some(raw);
        self.epoch.saturating_add(raw)
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Default)]
pub struct StreamDemuxer {
    bytes: Vec<u8>,
    continuity: HashMap<u16, u8>,
    sections: HashMap<u16, SectionAssembler>,
    pes: HashMap<u16, PesAssembler>,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    audio_pid: Option<u16>,
    video_pts: TimestampUnwrapper,
    video_dts: TimestampUnwrapper,
    audio_pts: TimestampUnwrapper,
    pending_discontinuity: HashMap<u16, bool>,
}

impl StreamDemuxer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, input: &[u8]) -> Result<Vec<DemuxEvent>, TsError> {
        self.bytes.extend_from_slice(input);
        let mut events = Vec::new();

        loop {
            if self.bytes.len() < PACKET_SIZE {
                break;
            }
            if self.bytes[0] != SYNC_BYTE {
                if let Some(offset) = find_next_sync(&self.bytes) {
                    self.bytes.drain(..offset);
                    events.push(DemuxEvent::SyncRecovered {
                        discarded_bytes: offset,
                    });
                } else {
                    let keep = PACKET_SIZE - 1;
                    let discarded = self.bytes.len().saturating_sub(keep);
                    self.bytes.drain(..discarded);
                    events.push(DemuxEvent::SyncRecovered {
                        discarded_bytes: discarded,
                    });
                    break;
                }
            }

            let raw = self.bytes[..PACKET_SIZE].to_vec();
            self.bytes.drain(..PACKET_SIZE);
            let pid = (u16::from(raw[1] & 0x1f) << 8) | u16::from(raw[2]);
            match TsPacket::parse(&raw) {
                Ok(packet) => {
                    if let Err(error) = self.process_packet(packet, &mut events) {
                        self.reset_pid(pid);
                        self.pending_discontinuity.insert(pid, true);
                        events.push(DemuxEvent::CorruptData {
                            pid: Some(pid),
                            reason: error.to_string(),
                        });
                    }
                }
                Err(error) => {
                    self.reset_pid(pid);
                    self.pending_discontinuity.insert(pid, true);
                    events.push(DemuxEvent::CorruptData {
                        pid: Some(pid),
                        reason: error.to_string(),
                    });
                }
            }
        }

        // A hostile stream must not grow memory forever while never producing a full packet.
        if self.bytes.len() > PACKET_SIZE * 3 {
            let keep = PACKET_SIZE - 1;
            let discarded = self.bytes.len() - keep;
            self.bytes.drain(..discarded);
            events.push(DemuxEvent::SyncRecovered {
                discarded_bytes: discarded,
            });
        }
        Ok(events)
    }

    pub fn flush(&mut self) -> Result<Vec<DemuxEvent>, TsError> {
        let mut events = Vec::new();
        for pid in [self.video_pid, self.audio_pid].into_iter().flatten() {
            if let Some(pes) = self.pes.get_mut(&pid).and_then(PesAssembler::finish) {
                if let Err(error) = self.emit_pes(pid, pes, &mut events) {
                    self.reset_pid(pid);
                    events.push(DemuxEvent::CorruptData {
                        pid: Some(pid),
                        reason: error.to_string(),
                    });
                }
            }
        }
        Ok(events)
    }

    fn process_packet(
        &mut self,
        packet: TsPacket<'_>,
        events: &mut Vec<DemuxEvent>,
    ) -> Result<(), TsError> {
        let pid = packet.header.pid;
        if packet.header.transport_error {
            self.reset_pid(pid);
            self.pending_discontinuity.insert(pid, true);
            events.push(DemuxEvent::Discontinuity { pid });
            return Ok(());
        }

        let discontinuity = packet
            .adaptation
            .is_some_and(|adaptation| adaptation.discontinuity);
        if packet.header.adaptation_control.has_payload() {
            if let Some(previous) = self
                .continuity
                .insert(pid, packet.header.continuity_counter)
            {
                let expected = (previous + 1) & 0x0f;
                if !discontinuity && packet.header.continuity_counter != expected {
                    self.reset_pid(pid);
                    self.pending_discontinuity.insert(pid, true);
                    events.push(DemuxEvent::ContinuityError {
                        pid,
                        expected,
                        actual: packet.header.continuity_counter,
                    });
                }
            }
        }
        if discontinuity {
            self.reset_pid(pid);
            self.pending_discontinuity.insert(pid, true);
            events.push(DemuxEvent::Discontinuity { pid });
        }
        if packet.payload.is_empty() {
            return Ok(());
        }

        if pid == PAT_PID || Some(pid) == self.pmt_pid {
            let sections = self
                .sections
                .entry(pid)
                .or_default()
                .push(packet.payload, packet.header.payload_unit_start);
            for section in sections {
                self.process_section(pid, &section, events)?;
            }
        } else if Some(pid) == self.video_pid || Some(pid) == self.audio_pid {
            let assembler = self.pes.entry(pid).or_default();
            if packet.header.payload_unit_start {
                if let Some(previous) = assembler.start(packet.payload)? {
                    self.emit_pes(pid, previous, events)?;
                }
            } else {
                assembler.push(packet.payload)?;
            }
            while let Some(complete) = self.pes.get_mut(&pid).and_then(PesAssembler::take_complete)
            {
                self.emit_pes(pid, complete, events)?;
            }
        }
        Ok(())
    }

    fn process_section(
        &mut self,
        pid: u16,
        section: &[u8],
        events: &mut Vec<DemuxEvent>,
    ) -> Result<(), TsError> {
        validate_section_crc(section)?;
        if pid == PAT_PID && section.first() == Some(&0x00) {
            self.pmt_pid = parse_pat_pmt_pid(section);
        } else if Some(pid) == self.pmt_pid && section.first() == Some(&0x02) {
            let map = parse_program_map(section, pid)?;
            self.video_pid = Some(map.video_pid);
            self.audio_pid = Some(map.audio_pid);
            events.push(DemuxEvent::ProgramMap(map));
        }
        Ok(())
    }

    fn emit_pes(
        &mut self,
        pid: u16,
        data: Vec<u8>,
        events: &mut Vec<DemuxEvent>,
    ) -> Result<(), TsError> {
        let parsed = parse_pes(&data)?;
        let discontinuity = self.pending_discontinuity.remove(&pid).unwrap_or(false);
        if Some(pid) == self.video_pid {
            let pts = self.video_pts.unwrap(parsed.pts);
            let dts = parsed.dts.map(|value| self.video_dts.unwrap(value));
            let keyframe = parse_annex_b(parsed.payload)
                .map(|units| units.iter().any(|unit| unit.nal_unit_type == 5))
                .unwrap_or(false);
            events.push(DemuxEvent::Packet(ElementaryPacket {
                stream: MuxStream::Video,
                pid,
                pts_90khz: pts,
                dts_90khz: dts,
                keyframe,
                discontinuity,
                data: Bytes::copy_from_slice(parsed.payload),
            }));
        } else if Some(pid) == self.audio_pid {
            let base_pts = self.audio_pts.unwrap(parsed.pts);
            let frames = parse_adts_stream(parsed.payload)
                .map_err(|_| TsError::MalformedPes("invalid AAC ADTS payload"))?;
            let mut offset = 0_u64;
            for frame in frames {
                events.push(DemuxEvent::Packet(ElementaryPacket {
                    stream: MuxStream::Audio,
                    pid,
                    pts_90khz: base_pts.saturating_add(offset),
                    dts_90khz: None,
                    keyframe: true,
                    discontinuity: discontinuity && offset == 0,
                    data: Bytes::copy_from_slice(frame.bytes),
                }));
                let samples = 1024_u64 * (u64::from(frame.header.raw_data_blocks) + 1);
                offset = offset.saturating_add(samples * 90_000 / 48_000);
            }
        }
        Ok(())
    }

    fn reset_pid(&mut self, pid: u16) {
        self.continuity.remove(&pid);
        self.sections.entry(pid).or_default().reset();
        self.pes.entry(pid).or_default().reset();
        if Some(pid) == self.video_pid {
            self.video_pts.reset();
            self.video_dts.reset();
        } else if Some(pid) == self.audio_pid {
            self.audio_pts.reset();
        }
    }
}

fn find_next_sync(data: &[u8]) -> Option<usize> {
    (1..data.len()).find(|offset| {
        data[*offset] == SYNC_BYTE
            && data
                .get(offset.saturating_add(PACKET_SIZE))
                .is_none_or(|next| *next == SYNC_BYTE)
    })
}

fn validate_section_crc(section: &[u8]) -> Result<(), TsError> {
    if section.len() < 4 {
        return Err(TsError::TruncatedSection);
    }
    if mpeg_crc32(section) != 0 {
        return Err(TsError::InvalidSectionCrc);
    }
    Ok(())
}

fn parse_pat_pmt_pid(section: &[u8]) -> Option<u16> {
    let end = section.len().checked_sub(4)?;
    let mut cursor = 8;
    while cursor + 4 <= end {
        let program = u16::from_be_bytes([section[cursor], section[cursor + 1]]);
        let pid = (u16::from(section[cursor + 2] & 0x1f) << 8) | u16::from(section[cursor + 3]);
        if program != 0 {
            return Some(pid);
        }
        cursor += 4;
    }
    None
}

fn parse_program_map(section: &[u8], pmt_pid: u16) -> Result<ProgramMap, TsError> {
    if section.len() < 16 {
        return Err(TsError::TruncatedSection);
    }
    let program_number = u16::from_be_bytes([section[3], section[4]]);
    let version = (section[5] >> 1) & 0x1f;
    let pcr_pid = (u16::from(section[8] & 0x1f) << 8) | u16::from(section[9]);
    let info_length = (usize::from(section[10] & 0x0f) << 8) | usize::from(section[11]);
    let mut cursor = 12 + info_length;
    let end = section.len() - 4;
    let mut video_pid = None;
    let mut audio_pid = None;
    while cursor + 5 <= end {
        let stream_type = section[cursor];
        let pid = (u16::from(section[cursor + 1] & 0x1f) << 8) | u16::from(section[cursor + 2]);
        let es_info_length =
            (usize::from(section[cursor + 3] & 0x0f) << 8) | usize::from(section[cursor + 4]);
        match stream_type {
            0x1b => {
                video_pid.get_or_insert(pid);
            }
            0x0f => {
                audio_pid.get_or_insert(pid);
            }
            _ => {}
        }
        cursor = cursor
            .checked_add(5 + es_info_length)
            .ok_or(TsError::TruncatedSection)?;
    }
    Ok(ProgramMap {
        program_number,
        pmt_pid,
        pcr_pid,
        video_pid: video_pid.ok_or(TsError::MalformedPes("PMT has no H.264 stream"))?,
        audio_pid: audio_pid.ok_or(TsError::MalformedPes("PMT has no AAC ADTS stream"))?,
        version,
    })
}

struct ParsedPes<'a> {
    pts: u64,
    dts: Option<u64>,
    payload: &'a [u8],
}

fn parse_pes(data: &[u8]) -> Result<ParsedPes<'_>, TsError> {
    if data.len() < 9 || !data.starts_with(&[0, 0, 1]) {
        return Err(TsError::MalformedPes("missing start code or fixed header"));
    }
    let flags = data[7];
    let header_length = usize::from(data[8]);
    let payload_start = 9_usize
        .checked_add(header_length)
        .ok_or(TsError::MalformedPes("header length overflow"))?;
    let payload = data
        .get(payload_start..)
        .ok_or(TsError::MalformedPes("truncated optional header"))?;
    let timestamp_flags = (flags >> 6) & 0x03;
    if timestamp_flags != 0x02 && timestamp_flags != 0x03 {
        return Err(TsError::MalformedPes("PTS is required"));
    }
    let pts = decode_timestamp(
        data.get(9..14)
            .ok_or(TsError::MalformedPes("truncated PTS"))?,
    )?;
    let dts = if timestamp_flags == 0x03 {
        Some(decode_timestamp(
            data.get(14..19)
                .ok_or(TsError::MalformedPes("truncated DTS"))?,
        )?)
    } else {
        None
    };
    Ok(ParsedPes { pts, dts, payload })
}

fn decode_timestamp(bytes: &[u8]) -> Result<u64, TsError> {
    if bytes.len() != 5 || bytes[0] & 1 == 0 || bytes[2] & 1 == 0 || bytes[4] & 1 == 0 {
        return Err(TsError::MalformedPes("invalid timestamp marker bits"));
    }
    Ok((u64::from((bytes[0] >> 1) & 0x07) << 30)
        | (u64::from(bytes[1]) << 22)
        | (u64::from(bytes[2] >> 1) << 15)
        | (u64::from(bytes[3]) << 7)
        | u64::from(bytes[4] >> 1))
}

#[derive(Debug)]
pub struct StreamMuxer {
    continuity: HashMap<u16, u8>,
    last_psi_pts: Option<u64>,
}

impl Default for StreamMuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamMuxer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            continuity: HashMap::new(),
            last_psi_pts: None,
        }
    }

    pub fn push(&mut self, packet: &MuxPacket) -> Result<Vec<u8>, TsError> {
        let mut output = Vec::new();
        if self
            .last_psi_pts
            .is_none_or(|previous| packet.pts_90khz.saturating_sub(previous) >= 9_000)
        {
            let pat = build_pat_section();
            let pmt = build_pmt_section();
            self.packetize_section(PAT_PID, &pat, &mut output);
            self.packetize_section(PMT_PID, &pmt, &mut output);
            self.last_psi_pts = Some(packet.pts_90khz);
        }

        let (pid, stream_id) = match packet.stream {
            MuxStream::Video => (VIDEO_PID, 0xe0),
            MuxStream::Audio => (AUDIO_PID, 0xc0),
        };
        let pes = build_pes(stream_id, packet)?;
        self.packetize_pes(
            pid,
            &pes,
            packet.stream == MuxStream::Video,
            packet.keyframe,
            packet.pts_90khz,
            &mut output,
        );
        Ok(output)
    }

    pub fn force_program_tables(&mut self) {
        self.last_psi_pts = None;
    }

    fn packetize_section(&mut self, pid: u16, section: &[u8], output: &mut Vec<u8>) {
        let mut payload = Vec::with_capacity(section.len() + 1);
        payload.push(0);
        payload.extend_from_slice(section);
        self.packetize_payload(pid, &payload, true, None, false, output);
    }

    fn packetize_pes(
        &mut self,
        pid: u16,
        pes: &[u8],
        include_pcr: bool,
        random_access: bool,
        pts: u64,
        output: &mut Vec<u8>,
    ) {
        self.packetize_payload(
            pid,
            pes,
            true,
            include_pcr.then_some(pts.saturating_mul(300)),
            random_access,
            output,
        );
    }

    fn packetize_payload(
        &mut self,
        pid: u16,
        payload: &[u8],
        payload_unit_start: bool,
        first_pcr_27mhz: Option<u64>,
        random_access: bool,
        output: &mut Vec<u8>,
    ) {
        let mut cursor = 0;
        let mut first = true;
        while cursor < payload.len() {
            let pcr = first_pcr_27mhz.filter(|_| first);
            let minimum_adaptation = if pcr.is_some() { 8 } else { 0 };
            let max_payload = 184_usize.saturating_sub(minimum_adaptation);
            let remaining = payload.len() - cursor;
            let take = remaining.min(max_payload);
            let needs_adaptation = pcr.is_some() || take < 184;
            let continuity = self.next_continuity(pid);
            let mut packet = [0xff_u8; PACKET_SIZE];
            packet[0] = SYNC_BYTE;
            packet[1] = ((pid >> 8) as u8) & 0x1f;
            if first && payload_unit_start {
                packet[1] |= 0x40;
            }
            packet[2] = pid as u8;
            packet[3] = continuity;

            let payload_offset = if needs_adaptation {
                packet[3] |= 0x30;
                let adaptation_total = 184 - take;
                packet[4] = (adaptation_total - 1) as u8;
                if adaptation_total > 1 {
                    let mut flags = 0_u8;
                    if random_access && first {
                        flags |= 0x40;
                    }
                    if pcr.is_some() {
                        flags |= 0x10;
                    }
                    packet[5] = flags;
                    if let Some(pcr) = pcr {
                        encode_pcr(pcr, &mut packet[6..12]);
                    }
                }
                4 + adaptation_total
            } else {
                packet[3] |= 0x10;
                4
            };
            packet[payload_offset..payload_offset + take]
                .copy_from_slice(&payload[cursor..cursor + take]);
            output.extend_from_slice(&packet);
            cursor += take;
            first = false;
        }
    }

    fn next_continuity(&mut self, pid: u16) -> u8 {
        let value = self.continuity.entry(pid).or_insert(0);
        let current = *value;
        *value = (*value + 1) & 0x0f;
        current
    }
}

fn build_pat_section() -> Vec<u8> {
    let mut section = vec![
        0x00,
        0xb0,
        0x0d,
        0x00,
        0x01,
        0xc1,
        0x00,
        0x00,
        0x00,
        0x01,
        0xe0 | ((PMT_PID >> 8) as u8 & 0x1f),
        PMT_PID as u8,
    ];
    append_crc(&mut section);
    section
}

fn build_pmt_section() -> Vec<u8> {
    let mut section = vec![
        0x02,
        0xb0,
        0x17,
        0x00,
        0x01,
        0xc1,
        0x00,
        0x00,
        0xe0 | ((VIDEO_PID >> 8) as u8 & 0x1f),
        VIDEO_PID as u8,
        0xf0,
        0x00,
        0x1b,
        0xe0 | ((VIDEO_PID >> 8) as u8 & 0x1f),
        VIDEO_PID as u8,
        0xf0,
        0x00,
        0x0f,
        0xe0 | ((AUDIO_PID >> 8) as u8 & 0x1f),
        AUDIO_PID as u8,
        0xf0,
        0x00,
    ];
    append_crc(&mut section);
    section
}

fn append_crc(section: &mut Vec<u8>) {
    let crc = mpeg_crc32(section);
    section.extend_from_slice(&crc.to_be_bytes());
}

fn mpeg_crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in bytes {
        crc ^= u32::from(*byte) << 24;
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

fn build_pes(stream_id: u8, packet: &MuxPacket) -> Result<Vec<u8>, TsError> {
    let has_dts = packet.dts_90khz.is_some_and(|dts| dts != packet.pts_90khz);
    let timestamp_bytes = if has_dts { 10 } else { 5 };
    let body_length = 3_usize
        .checked_add(timestamp_bytes)
        .and_then(|value| value.checked_add(packet.data.len()))
        .ok_or(TsError::MalformedPes("PES length overflow"))?;
    let declared_length = if body_length > usize::from(u16::MAX) {
        0
    } else {
        body_length as u16
    };
    let mut pes = Vec::with_capacity(6 + body_length);
    pes.extend_from_slice(&[0, 0, 1, stream_id]);
    pes.extend_from_slice(&declared_length.to_be_bytes());
    pes.push(0x80);
    pes.push(if has_dts { 0xc0 } else { 0x80 });
    pes.push(timestamp_bytes as u8);
    encode_timestamp(
        if has_dts { 0x03 } else { 0x02 },
        packet.pts_90khz,
        &mut pes,
    );
    if let Some(dts) = packet.dts_90khz.filter(|_| has_dts) {
        encode_timestamp(0x01, dts, &mut pes);
    }
    pes.extend_from_slice(&packet.data);
    Ok(pes)
}

fn encode_timestamp(prefix: u8, timestamp: u64, output: &mut Vec<u8>) {
    let value = timestamp & (PTS_MODULUS - 1);
    output.push((prefix << 4) | (((value >> 30) as u8 & 0x07) << 1) | 1);
    output.push((value >> 22) as u8);
    output.push((((value >> 15) as u8 & 0x7f) << 1) | 1);
    output.push((value >> 7) as u8);
    output.push(((value as u8 & 0x7f) << 1) | 1);
}

fn encode_pcr(pcr_27mhz: u64, output: &mut [u8]) {
    let base = (pcr_27mhz / 300) & (PTS_MODULUS - 1);
    let extension = pcr_27mhz % 300;
    output[0] = (base >> 25) as u8;
    output[1] = (base >> 17) as u8;
    output[2] = (base >> 9) as u8;
    output[3] = (base >> 1) as u8;
    output[4] = ((base as u8 & 1) << 7) | 0x7e | ((extension >> 8) as u8 & 1);
    output[5] = extension as u8;
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::{
        DemuxEvent, MAX_PES_BYTES, MuxPacket, MuxStream, PACKET_SIZE, PTS_MODULUS, PesAssembler,
        StreamDemuxer, StreamMuxer, TimestampUnwrapper,
    };

    #[test]
    fn mux_and_streaming_demux_round_trip_arbitrary_chunks() {
        let mut muxer = StreamMuxer::new();
        let video = MuxPacket {
            stream: MuxStream::Video,
            pts_90khz: 90_000,
            dts_90khz: None,
            keyframe: true,
            data: Bytes::from_static(&[0, 0, 0, 1, 0x65, 0x88, 0x84]),
        };
        let audio = MuxPacket {
            stream: MuxStream::Audio,
            pts_90khz: 90_000,
            dts_90khz: None,
            keyframe: true,
            data: Bytes::from_static(&[
                0xff, 0xf1, 0x4c, 0x80, 0x01, 0x7f, 0xfc, 0x11, 0x22, 0x33, 0x44,
            ]),
        };
        let mut transport = muxer.push(&video).expect("video muxes");
        transport.extend(muxer.push(&audio).expect("audio muxes"));

        let mut demuxer = StreamDemuxer::new();
        let mut events = Vec::new();
        for chunk in transport.chunks(37) {
            events.extend(demuxer.push(chunk).expect("chunk demuxes"));
        }
        events.extend(demuxer.flush().expect("demux flushes"));

        assert!(
            events
                .iter()
                .any(|event| matches!(event, DemuxEvent::ProgramMap(_)))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            DemuxEvent::Packet(packet)
                if packet.stream == MuxStream::Video && packet.keyframe
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            DemuxEvent::Packet(packet) if packet.stream == MuxStream::Audio
        )));
    }

    #[test]
    fn unwraps_the_33_bit_pts_rollover() {
        let mut unwrap = TimestampUnwrapper::default();
        assert_eq!(unwrap.unwrap(PTS_MODULUS - 10), PTS_MODULUS - 10);
        assert_eq!(unwrap.unwrap(20), PTS_MODULUS + 20);
    }

    #[test]
    fn garbage_input_stays_bounded_and_recovers_at_the_next_ts_packet() {
        let mut demuxer = StreamDemuxer::new();
        let events = demuxer
            .push(&vec![0_u8; PACKET_SIZE * 20])
            .expect("garbage is discarded");
        assert!(demuxer.bytes.len() < PACKET_SIZE);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DemuxEvent::SyncRecovered { .. }))
        );

        let mut muxer = StreamMuxer::new();
        let bytes = muxer
            .push(&MuxPacket {
                stream: MuxStream::Video,
                pts_90khz: 0,
                dts_90khz: None,
                keyframe: true,
                data: Bytes::from_static(&[0, 0, 1, 0x65, 0x01]),
            })
            .expect("video muxes");
        let events = demuxer.push(&bytes).expect("valid TS recovers");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DemuxEvent::ProgramMap(_)))
        );
    }

    #[test]
    fn corrupt_psi_is_reported_and_a_later_pat_pmt_recovers() {
        let mut muxer = StreamMuxer::new();
        let packet = |pts_90khz| MuxPacket {
            stream: MuxStream::Video,
            pts_90khz,
            dts_90khz: None,
            keyframe: true,
            data: Bytes::from_static(&[0, 0, 1, 0x65, 0x01]),
        };
        let mut damaged = muxer.push(&packet(0)).expect("first group muxes");
        let pat_payload = if damaged[3] & 0x20 != 0 {
            5 + usize::from(damaged[4])
        } else {
            4
        };
        damaged[pat_payload + 5] ^= 0x01;

        let mut demuxer = StreamDemuxer::new();
        let damaged_events = demuxer.push(&damaged).expect("damage is recoverable");
        assert!(
            damaged_events
                .iter()
                .any(|event| matches!(event, DemuxEvent::CorruptData { .. }))
        );

        let recovered = muxer.push(&packet(9_000)).expect("second group muxes");
        let recovered_events = demuxer.push(&recovered).expect("new tables recover");
        assert!(
            recovered_events
                .iter()
                .any(|event| matches!(event, DemuxEvent::ProgramMap(_)))
        );
    }

    #[test]
    fn pes_reassembly_has_a_hard_memory_limit() {
        let mut assembler = PesAssembler::default();
        assembler.start(&[0]).expect("small PES starts");
        let overflow = assembler.push(&vec![0_u8; MAX_PES_BYTES]);
        assert!(overflow.is_err());
        assert!(assembler.data.is_empty());
    }
}

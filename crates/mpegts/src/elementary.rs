//! Minimal clean-room parsers for the Alpha elementary stream formats.

use thiserror::Error;

const ADTS_SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ElementaryError {
    #[error("no Annex-B start code was found")]
    AnnexBStartCodeNotFound,
    #[error("Annex-B start code at byte {offset} has no NAL unit")]
    EmptyNalUnit { offset: usize },
    #[error("H.264 forbidden_zero_bit is set at byte {offset}")]
    ForbiddenZeroBit { offset: usize },
    #[error("ADTS header is truncated: need at least 7 bytes, got {0}")]
    TruncatedAdtsHeader(usize),
    #[error("invalid ADTS sync word")]
    InvalidAdtsSync,
    #[error("ADTS layer must be zero")]
    InvalidAdtsLayer,
    #[error("ADTS sample-frequency index {0} is reserved")]
    InvalidAdtsSampleRate(u8),
    #[error("ADTS channel configuration 0 requires a program config element")]
    UnsupportedAdtsProgramConfig,
    #[error("ADTS frame length {frame_length} is smaller than header length {header_length}")]
    InvalidAdtsFrameLength {
        frame_length: usize,
        header_length: usize,
    },
    #[error("ADTS frame is truncated: header declares {declared} bytes, got {available}")]
    TruncatedAdtsFrame { declared: usize, available: usize },
}

#[derive(Debug, Clone, Copy)]
pub struct AnnexBNalUnit<'a> {
    pub offset: usize,
    pub start_code_length: usize,
    pub nal_unit_type: u8,
    pub nal_ref_idc: u8,
    pub bytes: &'a [u8],
}

/// Splits an H.264 Annex-B byte stream into NAL units without decoding RBSP data.
pub fn parse_annex_b(data: &[u8]) -> Result<Vec<AnnexBNalUnit<'_>>, ElementaryError> {
    let mut units = Vec::new();
    let Some((mut start, mut start_code_length)) = find_start_code(data, 0) else {
        return Err(ElementaryError::AnnexBStartCodeNotFound);
    };

    loop {
        let nal_start = start + start_code_length;
        let next = find_start_code(data, nal_start);
        let nal_end = next.map_or(data.len(), |(offset, _)| offset);
        let bytes = data
            .get(nal_start..nal_end)
            .filter(|bytes| !bytes.is_empty())
            .ok_or(ElementaryError::EmptyNalUnit { offset: start })?;
        let header = bytes[0];
        if header & 0x80 != 0 {
            return Err(ElementaryError::ForbiddenZeroBit { offset: nal_start });
        }
        units.push(AnnexBNalUnit {
            offset: start,
            start_code_length,
            nal_unit_type: header & 0x1f,
            nal_ref_idc: (header >> 5) & 0x03,
            bytes,
        });

        let Some((next_start, next_length)) = next else {
            break;
        };
        start = next_start;
        start_code_length = next_length;
    }
    Ok(units)
}

fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut offset = from;
    while offset + 3 <= data.len() {
        if data[offset..].starts_with(&[0, 0, 1]) {
            return Some((offset, 3));
        }
        if data[offset..].starts_with(&[0, 0, 0, 1]) {
            return Some((offset, 4));
        }
        offset += 1;
    }
    None
}

#[derive(Debug, Clone, Copy)]
pub struct AdtsHeader {
    pub mpeg_version: u8,
    pub audio_object_type: u8,
    pub sample_rate_hz: u32,
    pub channel_configuration: u8,
    pub frame_length: usize,
    pub header_length: usize,
    pub raw_data_blocks: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct AdtsFrame<'a> {
    pub header: AdtsHeader,
    pub bytes: &'a [u8],
    pub payload: &'a [u8],
}

/// Parses one complete AAC ADTS frame and returns its header and borrowed payload.
pub fn parse_adts_frame(data: &[u8]) -> Result<AdtsFrame<'_>, ElementaryError> {
    if data.len() < 7 {
        return Err(ElementaryError::TruncatedAdtsHeader(data.len()));
    }
    if data[0] != 0xff || data[1] & 0xf0 != 0xf0 {
        return Err(ElementaryError::InvalidAdtsSync);
    }
    if data[1] & 0x06 != 0 {
        return Err(ElementaryError::InvalidAdtsLayer);
    }

    let protection_absent = data[1] & 0x01 != 0;
    let header_length = if protection_absent { 7 } else { 9 };
    if data.len() < header_length {
        return Err(ElementaryError::TruncatedAdtsHeader(data.len()));
    }
    let sample_rate_index = (data[2] >> 2) & 0x0f;
    let sample_rate_hz = ADTS_SAMPLE_RATES
        .get(usize::from(sample_rate_index))
        .copied()
        .ok_or(ElementaryError::InvalidAdtsSampleRate(sample_rate_index))?;
    let channel_configuration = ((data[2] & 0x01) << 2) | (data[3] >> 6);
    if channel_configuration == 0 {
        return Err(ElementaryError::UnsupportedAdtsProgramConfig);
    }
    let frame_length = (usize::from(data[3] & 0x03) << 11)
        | (usize::from(data[4]) << 3)
        | usize::from(data[5] >> 5);
    if frame_length < header_length {
        return Err(ElementaryError::InvalidAdtsFrameLength {
            frame_length,
            header_length,
        });
    }
    if data.len() < frame_length {
        return Err(ElementaryError::TruncatedAdtsFrame {
            declared: frame_length,
            available: data.len(),
        });
    }

    let bytes = &data[..frame_length];
    Ok(AdtsFrame {
        header: AdtsHeader {
            mpeg_version: (data[1] >> 3) & 0x01,
            audio_object_type: ((data[2] >> 6) & 0x03) + 1,
            sample_rate_hz,
            channel_configuration,
            frame_length,
            header_length,
            raw_data_blocks: data[6] & 0x03,
        },
        bytes,
        payload: &bytes[header_length..],
    })
}

/// Parses a buffer containing only consecutive complete ADTS frames.
pub fn parse_adts_stream(mut data: &[u8]) -> Result<Vec<AdtsFrame<'_>>, ElementaryError> {
    let mut frames = Vec::new();
    while !data.is_empty() {
        let frame = parse_adts_frame(data)?;
        data = &data[frame.header.frame_length..];
        frames.push(frame);
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::{parse_adts_stream, parse_annex_b};

    #[test]
    fn splits_mixed_annex_b_start_codes() {
        let stream = [
            0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x1f, 0x00, 0x00, 0x00, 0x01, 0x65, 0x88,
        ];
        let units = parse_annex_b(&stream).expect("valid Annex-B stream");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].nal_unit_type, 7);
        assert_eq!(units[1].nal_unit_type, 5);
        assert_eq!(units[1].start_code_length, 4);
    }

    #[test]
    fn parses_aac_lc_48khz_stereo_adts() {
        let stream = [
            0xff, 0xf1, 0x4c, 0x80, 0x01, 0x7f, 0xfc, 0x11, 0x22, 0x33, 0x44,
        ];
        let frames = parse_adts_stream(&stream).expect("valid ADTS stream");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].header.audio_object_type, 2);
        assert_eq!(frames[0].header.sample_rate_hz, 48_000);
        assert_eq!(frames[0].header.channel_configuration, 2);
        assert_eq!(frames[0].payload, &[0x11, 0x22, 0x33, 0x44]);
    }
}

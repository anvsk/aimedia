use aimedia_mpegts::elementary::parse_annex_b;
use bytes::Bytes;
use shiguredo_rtmp::AvcSequenceHeader;
use thiserror::Error;

use crate::{RawAvcPacketKind, RawVideoCodec, RawVideoFrame};

const ANNEX_B_START_CODE: [u8; 4] = [0, 0, 0, 1];
const MAX_ACCESS_UNIT_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONFIG_BYTES: usize = 256 * 1024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AvcError {
    #[error("RTMP video frame is not traditional H.264/AVC")]
    UnsupportedFormat,
    #[error("H.264 NAL data arrived before AVCDecoderConfigurationRecord")]
    MissingConfig,
    #[error("AVCDecoderConfigurationRecord is invalid")]
    InvalidConfig,
    #[error("AVCC NAL unit is empty, truncated, or exceeds the access-unit limit")]
    InvalidAvcc,
    #[error("Annex-B access unit is invalid")]
    InvalidAnnexB,
    #[error("FLV composition-time offset must fit a signed 24-bit integer")]
    InvalidCompositionOffset,
    #[error("an Annex-B IDR must include SPS and PPS before RTMP publishing can start")]
    MissingParameterSets,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcAccessUnit {
    pub dts_ms: u32,
    pub pts_ms: i64,
    pub keyframe: bool,
    pub annex_b: Bytes,
}

#[derive(Debug, Clone)]
struct DecoderConfig {
    nalu_length_bytes: usize,
    sps: Vec<Bytes>,
    pps: Vec<Bytes>,
}

#[derive(Debug, Default)]
pub struct AvcIngest {
    config: Option<DecoderConfig>,
    waiting_for_idr: bool,
}

impl AvcIngest {
    pub fn push(&mut self, frame: &RawVideoFrame) -> Result<Option<AvcAccessUnit>, AvcError> {
        if frame.codec != RawVideoCodec::Avc {
            return Err(AvcError::UnsupportedFormat);
        }
        match frame.packet_kind {
            Some(RawAvcPacketKind::SequenceHeader) => {
                self.config = Some(parse_decoder_config(&frame.payload)?);
                self.waiting_for_idr = true;
                Ok(None)
            }
            Some(RawAvcPacketKind::EndOfSequence) => {
                self.waiting_for_idr = true;
                Ok(None)
            }
            Some(RawAvcPacketKind::NalUnit) => self.push_nal_units(frame),
            None => Err(AvcError::UnsupportedFormat),
        }
    }

    fn push_nal_units(&mut self, frame: &RawVideoFrame) -> Result<Option<AvcAccessUnit>, AvcError> {
        if frame.payload.len() > MAX_ACCESS_UNIT_BYTES {
            return Err(AvcError::InvalidAvcc);
        }
        let config = self.config.as_ref().ok_or(AvcError::MissingConfig)?;
        let units = parse_avcc_units(&frame.payload, config.nalu_length_bytes)?;
        let has_idr = units.iter().any(|unit| unit[0] & 0x1f == 5);
        if self.waiting_for_idr && !has_idr {
            return Ok(None);
        }

        let parameter_set_bytes = config
            .sps
            .iter()
            .chain(config.pps.iter())
            .map(|unit| ANNEX_B_START_CODE.len() + unit.len())
            .sum::<usize>();
        let media_bytes = units
            .iter()
            .filter(|unit| !matches!(unit[0] & 0x1f, 7 | 8))
            .map(|unit| ANNEX_B_START_CODE.len() + unit.len())
            .sum::<usize>();
        let prefix_bytes = if self.waiting_for_idr {
            parameter_set_bytes
        } else {
            0
        };
        if prefix_bytes.saturating_add(media_bytes) > MAX_ACCESS_UNIT_BYTES {
            return Err(AvcError::InvalidAvcc);
        }

        let mut annex_b = Vec::with_capacity(prefix_bytes + media_bytes);
        if self.waiting_for_idr {
            for unit in config.sps.iter().chain(config.pps.iter()) {
                annex_b.extend_from_slice(&ANNEX_B_START_CODE);
                annex_b.extend_from_slice(unit);
            }
        }
        for unit in units
            .into_iter()
            .filter(|unit| !matches!(unit[0] & 0x1f, 7 | 8))
        {
            annex_b.extend_from_slice(&ANNEX_B_START_CODE);
            annex_b.extend_from_slice(unit);
        }
        if annex_b.is_empty() {
            return Err(AvcError::InvalidAvcc);
        }
        if has_idr {
            self.waiting_for_idr = false;
        }

        Ok(Some(AvcAccessUnit {
            dts_ms: frame.timestamp_ms,
            pts_ms: i64::from(frame.timestamp_ms) + i64::from(frame.composition_offset_ms),
            keyframe: has_idr,
            annex_b: Bytes::from(annex_b),
        }))
    }

    pub const fn waiting_for_idr(&self) -> bool {
        self.waiting_for_idr
    }

    pub fn reset(&mut self) {
        self.config = None;
        self.waiting_for_idr = false;
    }
}

#[derive(Debug, Default)]
pub struct AvcPublisher {
    sps: Vec<Bytes>,
    pps: Vec<Bytes>,
    sequence_header_sent: bool,
}

impl AvcPublisher {
    pub fn push_annex_b(
        &mut self,
        dts_ms: u32,
        composition_offset_ms: i32,
        annex_b: &[u8],
    ) -> Result<Vec<RawVideoFrame>, AvcError> {
        if annex_b.len() > MAX_ACCESS_UNIT_BYTES {
            return Err(AvcError::InvalidAnnexB);
        }
        if !(-8_388_608..=8_388_607).contains(&composition_offset_ms) {
            return Err(AvcError::InvalidCompositionOffset);
        }
        let units = parse_annex_b(annex_b).map_err(|_| AvcError::InvalidAnnexB)?;
        let mut next_sps = Vec::new();
        let mut next_pps = Vec::new();
        let mut media = Vec::new();
        let mut has_idr = false;

        for unit in units {
            match unit.nal_unit_type {
                7 => next_sps.push(Bytes::copy_from_slice(unit.bytes)),
                8 => next_pps.push(Bytes::copy_from_slice(unit.bytes)),
                5 => {
                    has_idr = true;
                    media.push(unit.bytes);
                }
                _ => media.push(unit.bytes),
            }
        }

        let mut config_changed = false;
        if !next_sps.is_empty() && next_sps != self.sps {
            self.sps = next_sps;
            config_changed = true;
        }
        if !next_pps.is_empty() && next_pps != self.pps {
            self.pps = next_pps;
            config_changed = true;
        }
        if config_changed {
            self.sequence_header_sent = false;
        }
        if (has_idr || config_changed) && (self.sps.is_empty() || self.pps.is_empty()) {
            return Err(AvcError::MissingParameterSets);
        }

        let mut output = Vec::with_capacity(2);
        if !self.sequence_header_sent && !self.sps.is_empty() && !self.pps.is_empty() {
            output.push(RawVideoFrame {
                timestamp_ms: dts_ms,
                composition_offset_ms: 0,
                codec: RawVideoCodec::Avc,
                packet_kind: Some(RawAvcPacketKind::SequenceHeader),
                keyframe: true,
                payload: build_sequence_header(&self.sps, &self.pps)?,
            });
            self.sequence_header_sent = true;
        }
        if !media.is_empty() {
            let avcc_len = media
                .iter()
                .map(|unit| 4_usize.saturating_add(unit.len()))
                .sum::<usize>();
            if avcc_len > MAX_ACCESS_UNIT_BYTES {
                return Err(AvcError::InvalidAnnexB);
            }
            let mut avcc = Vec::with_capacity(avcc_len);
            for unit in media {
                let length = u32::try_from(unit.len()).map_err(|_| AvcError::InvalidAnnexB)?;
                avcc.extend_from_slice(&length.to_be_bytes());
                avcc.extend_from_slice(unit);
            }
            output.push(RawVideoFrame {
                timestamp_ms: dts_ms,
                composition_offset_ms,
                codec: RawVideoCodec::Avc,
                packet_kind: Some(RawAvcPacketKind::NalUnit),
                keyframe: has_idr,
                payload: Bytes::from(avcc),
            });
        }
        Ok(output)
    }

    pub fn reset(&mut self) {
        self.sps.clear();
        self.pps.clear();
        self.sequence_header_sent = false;
    }
}

fn parse_decoder_config(data: &[u8]) -> Result<DecoderConfig, AvcError> {
    if data.len() > MAX_CONFIG_BYTES {
        return Err(AvcError::InvalidConfig);
    }
    let config = AvcSequenceHeader::from_bytes(data).map_err(|_| AvcError::InvalidConfig)?;
    let nalu_length_bytes = usize::from(config.length_size_minus_one) + 1;
    if !matches!(nalu_length_bytes, 1 | 2 | 4) {
        return Err(AvcError::InvalidConfig);
    }
    Ok(DecoderConfig {
        nalu_length_bytes,
        sps: config.sps_list.into_iter().map(Bytes::from).collect(),
        pps: config.pps_list.into_iter().map(Bytes::from).collect(),
    })
}

fn parse_avcc_units(data: &[u8], length_bytes: usize) -> Result<Vec<&[u8]>, AvcError> {
    let mut units = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let length_field = data
            .get(offset..offset + length_bytes)
            .ok_or(AvcError::InvalidAvcc)?;
        let mut length = 0_usize;
        for byte in length_field {
            length = length << 8 | usize::from(*byte);
        }
        offset += length_bytes;
        let unit = data
            .get(offset..offset.saturating_add(length))
            .filter(|unit| !unit.is_empty())
            .ok_or(AvcError::InvalidAvcc)?;
        if unit[0] & 0x80 != 0 {
            return Err(AvcError::InvalidAvcc);
        }
        units.push(unit);
        offset += length;
    }
    if units.is_empty() {
        return Err(AvcError::InvalidAvcc);
    }
    Ok(units)
}

fn build_sequence_header(sps: &[Bytes], pps: &[Bytes]) -> Result<Bytes, AvcError> {
    let first_sps = sps
        .first()
        .filter(|unit| unit.len() >= 4)
        .ok_or(AvcError::InvalidAnnexB)?;
    let config = AvcSequenceHeader {
        avc_profile_indication: first_sps[1],
        profile_compatibility: first_sps[2],
        avc_level_indication: first_sps[3],
        length_size_minus_one: 3,
        sps_list: sps.iter().map(|unit| unit.to_vec()).collect(),
        pps_list: pps.iter().map(|unit| unit.to_vec()).collect(),
    };
    config
        .to_bytes()
        .map(Bytes::from)
        .map_err(|_| AvcError::InvalidAnnexB)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9];
    const PPS: &[u8] = &[0x68, 0xee, 0x3c, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84];

    #[test]
    fn annex_b_and_avcc_round_trip_preserves_parameter_sets_and_timestamps() {
        let annex_b = [
            ANNEX_B_START_CODE.as_slice(),
            SPS,
            ANNEX_B_START_CODE.as_slice(),
            PPS,
            ANNEX_B_START_CODE.as_slice(),
            IDR,
        ]
        .concat();
        let mut publisher = AvcPublisher::default();
        let frames = publisher.push_annex_b(100, 7, &annex_b).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0].packet_kind,
            Some(RawAvcPacketKind::SequenceHeader)
        );
        assert_eq!(frames[1].packet_kind, Some(RawAvcPacketKind::NalUnit));

        let mut ingest = AvcIngest::default();
        assert!(ingest.push(&frames[0]).unwrap().is_none());
        let access_unit = ingest.push(&frames[1]).unwrap().unwrap();
        assert_eq!(access_unit.dts_ms, 100);
        assert_eq!(access_unit.pts_ms, 107);
        assert!(access_unit.keyframe);
        assert!(
            access_unit
                .annex_b
                .windows(SPS.len())
                .any(|window| window == SPS)
        );
        assert!(
            access_unit
                .annex_b
                .windows(PPS.len())
                .any(|window| window == PPS)
        );
        assert!(
            access_unit
                .annex_b
                .windows(IDR.len())
                .any(|window| window == IDR)
        );
    }

    #[test]
    fn ingest_waits_for_idr_after_each_sequence_header() {
        let config = AvcSequenceHeader {
            avc_profile_indication: 0x64,
            profile_compatibility: 0,
            avc_level_indication: 0x1f,
            length_size_minus_one: 3,
            sps_list: vec![SPS.to_vec()],
            pps_list: vec![PPS.to_vec()],
        }
        .to_bytes()
        .unwrap();
        let mut ingest = AvcIngest::default();
        ingest
            .push(&RawVideoFrame {
                timestamp_ms: 0,
                composition_offset_ms: 0,
                codec: RawVideoCodec::Avc,
                packet_kind: Some(RawAvcPacketKind::SequenceHeader),
                keyframe: true,
                payload: Bytes::from(config),
            })
            .unwrap();
        let inter = RawVideoFrame {
            timestamp_ms: 33,
            composition_offset_ms: 0,
            codec: RawVideoCodec::Avc,
            packet_kind: Some(RawAvcPacketKind::NalUnit),
            keyframe: false,
            payload: Bytes::from_static(&[0, 0, 0, 2, 0x41, 0x01]),
        };
        assert!(ingest.push(&inter).unwrap().is_none());
        assert!(ingest.waiting_for_idr());
    }
}

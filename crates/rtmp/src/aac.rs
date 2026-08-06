use aimedia_mpegts::elementary::parse_adts_frame;
use bytes::Bytes;
use thiserror::Error;

use crate::RawAudioFrame;

const AAC_LC_OBJECT_TYPE: u8 = 2;
const SAMPLE_RATE_INDEX_48_KHZ: u8 = 3;
const CHANNELS_STEREO: u8 = 2;
const MAX_ADTS_FRAME_BYTES: usize = 0x1fff;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AacError {
    #[error("RTMP audio frame is not AAC")]
    UnsupportedFormat,
    #[error("AAC raw data arrived before AudioSpecificConfig")]
    MissingConfig,
    #[error("AAC AudioSpecificConfig is invalid or outside the 48 kHz stereo AAC-LC profile")]
    InvalidConfig,
    #[error("AAC raw payload is empty or exceeds the ADTS 13-bit frame length")]
    InvalidPayloadLength,
    #[error("AAC ADTS input must contain exactly one 48 kHz stereo AAC-LC frame")]
    InvalidAdts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AacAccessUnit {
    pub timestamp_ms: u32,
    pub adts: Bytes,
}

#[derive(Debug, Default)]
pub struct AacIngest {
    configured: bool,
}

impl AacIngest {
    pub fn push(&mut self, frame: &RawAudioFrame) -> Result<Option<AacAccessUnit>, AacError> {
        if !frame.aac {
            return Err(AacError::UnsupportedFormat);
        }
        if frame.sequence_header {
            parse_audio_specific_config(&frame.payload)?;
            self.configured = true;
            return Ok(None);
        }
        if !self.configured {
            return Err(AacError::MissingConfig);
        }
        if frame.payload.is_empty() || frame.payload.len() + 7 > MAX_ADTS_FRAME_BYTES {
            return Err(AacError::InvalidPayloadLength);
        }

        let frame_length = frame.payload.len() + 7;
        let mut adts = Vec::with_capacity(frame_length);
        adts.extend_from_slice(&[
            0xff,
            0xf1,
            (AAC_LC_OBJECT_TYPE - 1) << 6 | SAMPLE_RATE_INDEX_48_KHZ << 2 | CHANNELS_STEREO >> 2,
            (CHANNELS_STEREO & 0x03) << 6 | ((frame_length >> 11) & 0x03) as u8,
            ((frame_length >> 3) & 0xff) as u8,
            ((frame_length & 0x07) << 5) as u8 | 0x1f,
            0xfc,
        ]);
        adts.extend_from_slice(&frame.payload);
        Ok(Some(AacAccessUnit {
            timestamp_ms: frame.timestamp_ms,
            adts: Bytes::from(adts),
        }))
    }

    pub const fn configured(&self) -> bool {
        self.configured
    }

    pub fn reset(&mut self) {
        self.configured = false;
    }
}

#[derive(Debug, Default)]
pub struct AacPublisher {
    sequence_header_sent: bool,
}

impl AacPublisher {
    pub fn push_adts(
        &mut self,
        timestamp_ms: u32,
        adts: &[u8],
    ) -> Result<Vec<RawAudioFrame>, AacError> {
        let frame = parse_adts_frame(adts).map_err(|_| AacError::InvalidAdts)?;
        if frame.bytes.len() != adts.len()
            || frame.header.audio_object_type != AAC_LC_OBJECT_TYPE
            || frame.header.sample_rate_hz != 48_000
            || frame.header.channel_configuration != CHANNELS_STEREO
            || frame.header.raw_data_blocks != 0
            || frame.payload.is_empty()
        {
            return Err(AacError::InvalidAdts);
        }

        let mut output = Vec::with_capacity(if self.sequence_header_sent { 1 } else { 2 });
        if !self.sequence_header_sent {
            output.push(RawAudioFrame {
                timestamp_ms,
                aac: true,
                sequence_header: true,
                payload: Bytes::from_static(&[0x11, 0x90]),
            });
            self.sequence_header_sent = true;
        }
        output.push(RawAudioFrame {
            timestamp_ms,
            aac: true,
            sequence_header: false,
            payload: Bytes::copy_from_slice(frame.payload),
        });
        Ok(output)
    }

    pub fn reset(&mut self) {
        self.sequence_header_sent = false;
    }
}

fn parse_audio_specific_config(data: &[u8]) -> Result<(), AacError> {
    if data.len() < 2 {
        return Err(AacError::InvalidConfig);
    }
    let audio_object_type = data[0] >> 3;
    let sample_rate_index = (data[0] & 0x07) << 1 | data[1] >> 7;
    let channel_configuration = (data[1] >> 3) & 0x0f;
    if audio_object_type != AAC_LC_OBJECT_TYPE
        || sample_rate_index != SAMPLE_RATE_INDEX_48_KHZ
        || channel_configuration != CHANNELS_STEREO
    {
        return Err(AacError::InvalidConfig);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_header_and_raw_aac_round_trip_through_adts() {
        let mut ingest = AacIngest::default();
        ingest
            .push(&RawAudioFrame {
                timestamp_ms: 0,
                aac: true,
                sequence_header: true,
                payload: Bytes::from_static(&[0x11, 0x90]),
            })
            .unwrap();
        let access_unit = ingest
            .push(&RawAudioFrame {
                timestamp_ms: 21,
                aac: true,
                sequence_header: false,
                payload: Bytes::from_static(&[0x11, 0x22, 0x33, 0x44]),
            })
            .unwrap()
            .unwrap();

        let mut publisher = AacPublisher::default();
        let output = publisher
            .push_adts(access_unit.timestamp_ms, &access_unit.adts)
            .unwrap();
        assert_eq!(output.len(), 2);
        assert!(output[0].sequence_header);
        assert_eq!(output[0].payload, Bytes::from_static(&[0x11, 0x90]));
        assert_eq!(
            output[1].payload,
            Bytes::from_static(&[0x11, 0x22, 0x33, 0x44])
        );
    }

    #[test]
    fn raw_aac_requires_a_valid_sequence_header() {
        let mut ingest = AacIngest::default();
        let raw = RawAudioFrame {
            timestamp_ms: 0,
            aac: true,
            sequence_header: false,
            payload: Bytes::from_static(&[1]),
        };
        assert_eq!(ingest.push(&raw).unwrap_err(), AacError::MissingConfig);

        let invalid = RawAudioFrame {
            sequence_header: true,
            payload: Bytes::from_static(&[0x12, 0x10]),
            ..raw
        };
        assert_eq!(ingest.push(&invalid).unwrap_err(), AacError::InvalidConfig);
    }
}

use std::collections::HashMap;

use crate::{RtmpError, RtmpErrorCode, RtmpErrorStage};

const HANDSHAKE_BYTES: usize = 1 + 1536 + 1536;
const INITIAL_CHUNK_BYTES: usize = 128;
const MAX_FEED_BYTES: usize = 64 * 1024;
const MAX_CHUNK_STREAMS: usize = 64;
const MAX_HEADER_BYTES: usize = 18;

#[derive(Debug, Clone, Copy)]
struct MessageHeader {
    length: usize,
    type_id: u8,
    extended_timestamp: bool,
}

#[derive(Debug)]
struct CurrentMessage {
    remaining: usize,
    type_id: u8,
    prefix: [u8; 4],
    prefix_len: usize,
}

impl CurrentMessage {
    const fn new(header: MessageHeader) -> Self {
        Self {
            remaining: header.length,
            type_id: header.type_id,
            prefix: [0; 4],
            prefix_len: 0,
        }
    }
}

#[derive(Debug, Default)]
struct ChunkStream {
    previous: Option<MessageHeader>,
    current: Option<CurrentMessage>,
}

#[derive(Debug)]
pub(crate) struct IngressGuard {
    handshake_remaining: usize,
    chunk_size: usize,
    max_message_bytes: usize,
    buffer: Vec<u8>,
    streams: HashMap<u32, ChunkStream>,
    messages_seen: u64,
    peak_buffer_bytes: usize,
}

impl IngressGuard {
    pub(crate) fn new(max_message_bytes: usize) -> Self {
        Self {
            handshake_remaining: HANDSHAKE_BYTES,
            chunk_size: INITIAL_CHUNK_BYTES,
            max_message_bytes,
            buffer: Vec::new(),
            streams: HashMap::new(),
            messages_seen: 0,
            peak_buffer_bytes: 0,
        }
    }

    pub(crate) fn inspect(&mut self, mut bytes: &[u8]) -> Result<(), RtmpError> {
        if bytes.len() > MAX_FEED_BYTES {
            return Err(RtmpError::new(
                RtmpErrorCode::ResourceLimit,
                RtmpErrorStage::Receive,
                false,
                "one RTMP feed exceeds the 64 KiB ingress limit",
            ));
        }

        if self.handshake_remaining != 0 {
            let consumed = bytes.len().min(self.handshake_remaining);
            self.handshake_remaining -= consumed;
            bytes = &bytes[consumed..];
            if bytes.is_empty() {
                return Ok(());
            }
        }

        let buffer_limit = self.max_message_bytes.saturating_add(MAX_HEADER_BYTES);
        if self.buffer.len().saturating_add(bytes.len()) > buffer_limit {
            return Err(RtmpError::new(
                RtmpErrorCode::ResourceLimit,
                RtmpErrorStage::Receive,
                false,
                "incomplete RTMP chunk exceeds the bounded ingress buffer",
            ));
        }
        self.buffer.extend_from_slice(bytes);
        self.peak_buffer_bytes = self.peak_buffer_bytes.max(self.buffer.len());

        let mut consumed = 0;
        while let Some(chunk_bytes) = self.inspect_one_chunk(consumed)? {
            consumed += chunk_bytes;
        }
        if consumed != 0 {
            self.buffer.drain(..consumed);
        }
        Ok(())
    }

    fn inspect_one_chunk(&mut self, offset: usize) -> Result<Option<usize>, RtmpError> {
        let bytes = &self.buffer[offset..];
        let Some((&first, _)) = bytes.split_first() else {
            return Ok(None);
        };
        let format = first >> 6;
        let Some((chunk_stream_id, basic_header_bytes)) = parse_basic_header(bytes) else {
            return Ok(None);
        };
        if !self.streams.contains_key(&chunk_stream_id) {
            if format != 0 {
                return Err(malformed(
                    "new chunk stream must start with a type-0 header",
                ));
            }
            if self.streams.len() >= MAX_CHUNK_STREAMS {
                return Err(RtmpError::new(
                    RtmpErrorCode::ResourceLimit,
                    RtmpErrorStage::Receive,
                    false,
                    "RTMP connection exceeds 64 chunk streams",
                ));
            }
            self.streams.insert(chunk_stream_id, ChunkStream::default());
        }
        let stream = self
            .streams
            .get(&chunk_stream_id)
            .expect("chunk stream was inserted or already existed");

        let mut header_bytes = basic_header_bytes;
        let new_header = match format {
            0 => {
                let Some(header) = bytes.get(header_bytes..header_bytes + 11) else {
                    return Ok(None);
                };
                if stream
                    .current
                    .as_ref()
                    .is_some_and(|message| message.remaining != 0)
                {
                    return Err(malformed("type-0 header interrupted an incomplete message"));
                }
                header_bytes += 11;
                Some(MessageHeader {
                    length: read_u24(&header[3..6]),
                    type_id: header[6],
                    extended_timestamp: read_u24(&header[..3]) == 0x00ff_ffff,
                })
            }
            1 => {
                let Some(header) = bytes.get(header_bytes..header_bytes + 7) else {
                    return Ok(None);
                };
                require_completed_message(stream)?;
                header_bytes += 7;
                Some(MessageHeader {
                    length: read_u24(&header[3..6]),
                    type_id: header[6],
                    extended_timestamp: read_u24(&header[..3]) == 0x00ff_ffff,
                })
            }
            2 => {
                let Some(header) = bytes.get(header_bytes..header_bytes + 3) else {
                    return Ok(None);
                };
                require_completed_message(stream)?;
                let previous = stream
                    .previous
                    .ok_or_else(|| malformed("type-2 header has no previous message header"))?;
                header_bytes += 3;
                Some(MessageHeader {
                    extended_timestamp: read_u24(header) == 0x00ff_ffff,
                    ..previous
                })
            }
            3 => None,
            _ => unreachable!("RTMP header format is encoded in two bits"),
        };

        let effective_header = new_header
            .or(stream.previous)
            .ok_or_else(|| malformed("type-3 header has no previous message header"))?;
        if effective_header.extended_timestamp {
            if bytes.get(header_bytes..header_bytes + 4).is_none() {
                return Ok(None);
            }
            header_bytes += 4;
        }
        if effective_header.length > self.max_message_bytes {
            return Err(RtmpError::new(
                RtmpErrorCode::MessageTooLarge,
                RtmpErrorStage::Receive,
                false,
                "declared RTMP message exceeds maxMessageBytes",
            ));
        }
        if effective_header.type_id == 1 && effective_header.length != 4 {
            return Err(malformed(
                "Set Chunk Size message must declare exactly four bytes",
            ));
        }

        let remaining = stream
            .current
            .as_ref()
            .map_or(effective_header.length, |message| message.remaining);
        let payload_bytes = remaining.min(self.chunk_size);
        if bytes
            .get(header_bytes..header_bytes.saturating_add(payload_bytes))
            .is_none()
        {
            return Ok(None);
        }

        let stream = self
            .streams
            .get_mut(&chunk_stream_id)
            .expect("validated chunk stream must still exist");
        if let Some(header) = new_header {
            stream.previous = Some(header);
            stream.current = Some(CurrentMessage::new(header));
        } else if stream.current.is_none() {
            stream.current = Some(CurrentMessage::new(effective_header));
        }

        let payload = &bytes[header_bytes..header_bytes + payload_bytes];
        let message = stream
            .current
            .as_mut()
            .expect("a parsed chunk always belongs to a current message");
        if message.type_id == 1 && message.prefix_len < message.prefix.len() {
            let copy_len = (message.prefix.len() - message.prefix_len).min(payload.len());
            message.prefix[message.prefix_len..message.prefix_len + copy_len]
                .copy_from_slice(&payload[..copy_len]);
            message.prefix_len += copy_len;
        }
        message.remaining -= payload_bytes;

        if message.remaining == 0 {
            if message.type_id == 1 {
                if message.prefix_len != 4 {
                    return Err(malformed("Set Chunk Size message must contain four bytes"));
                }
                let raw = u32::from_be_bytes(message.prefix);
                if raw & 0x8000_0000 != 0 {
                    return Err(malformed("Set Chunk Size reserved bit must be zero"));
                }
                let chunk_size = raw as usize;
                if chunk_size == 0 || chunk_size > self.max_message_bytes {
                    return Err(RtmpError::new(
                        RtmpErrorCode::ResourceLimit,
                        RtmpErrorStage::Receive,
                        false,
                        "peer RTMP chunk size is zero or exceeds maxMessageBytes",
                    ));
                }
                self.chunk_size = chunk_size;
            }
            stream.current = None;
            self.messages_seen = self.messages_seen.saturating_add(1);
        }

        Ok(Some(header_bytes + payload_bytes))
    }

    pub(crate) const fn messages_seen(&self) -> u64 {
        self.messages_seen
    }

    pub(crate) const fn peak_buffer_bytes(&self) -> usize {
        self.peak_buffer_bytes
    }

    pub(crate) fn chunk_streams(&self) -> usize {
        self.streams.len()
    }
}

fn parse_basic_header(bytes: &[u8]) -> Option<(u32, usize)> {
    let first = bytes[0];
    match first & 0x3f {
        0 => bytes.get(1).map(|value| (u32::from(*value) + 64, 2)),
        1 => {
            let header = bytes.get(1..3)?;
            Some((u32::from(header[0]) + u32::from(header[1]) * 256 + 64, 3))
        }
        value => Some((u32::from(value), 1)),
    }
}

fn require_completed_message(stream: &ChunkStream) -> Result<(), RtmpError> {
    if stream.previous.is_none() {
        return Err(malformed(
            "compressed header has no previous message header",
        ));
    }
    if stream
        .current
        .as_ref()
        .is_some_and(|message| message.remaining != 0)
    {
        return Err(malformed(
            "compressed header interrupted an incomplete message",
        ));
    }
    Ok(())
}

fn read_u24(bytes: &[u8]) -> usize {
    usize::from(bytes[0]) << 16 | usize::from(bytes[1]) << 8 | usize::from(bytes[2])
}

fn malformed(message: &'static str) -> RtmpError {
    RtmpError::new(
        RtmpErrorCode::MalformedData,
        RtmpErrorStage::Receive,
        false,
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_message_before_protocol_allocation() {
        let mut guard = IngressGuard::new(64 * 1024);
        guard.inspect(&vec![0; HANDSHAKE_BYTES]).unwrap();

        let mut header = vec![3, 0, 0, 0, 1, 0, 1, 8, 1, 0, 0, 0];
        header.push(0);
        let error = guard.inspect(&header).unwrap_err();
        assert_eq!(error.code, RtmpErrorCode::MessageTooLarge);
    }

    #[test]
    fn carries_a_chunk_header_across_arbitrary_feed_boundaries() {
        let mut guard = IngressGuard::new(64 * 1024);
        guard.inspect(&vec![0; HANDSHAKE_BYTES]).unwrap();
        let chunk = [3, 0, 0, 0, 0, 0, 4, 1, 1, 0, 0, 0, 0, 0, 16, 0];

        for byte in chunk {
            guard.inspect(&[byte]).unwrap();
        }

        assert_eq!(guard.messages_seen(), 1);
    }

    #[test]
    fn rejects_chunk_stream_spray_at_the_connection_boundary() {
        let mut guard = IngressGuard::new(64 * 1024);
        guard.inspect(&vec![0; HANDSHAKE_BYTES]).unwrap();

        for chunk_stream_id in 2_u32..66 {
            guard.inspect(&empty_message(chunk_stream_id)).unwrap();
        }
        let error = guard.inspect(&empty_message(66)).unwrap_err();

        assert_eq!(error.code, RtmpErrorCode::ResourceLimit);
        assert_eq!(guard.chunk_streams(), MAX_CHUNK_STREAMS);
    }

    fn empty_message(chunk_stream_id: u32) -> Vec<u8> {
        let mut chunk = if chunk_stream_id <= 63 {
            vec![chunk_stream_id as u8]
        } else {
            vec![0, (chunk_stream_id - 64) as u8]
        };
        chunk.extend_from_slice(&[0, 0, 0, 0, 0, 0, 8, 1, 0, 0, 0]);
        chunk
    }
}

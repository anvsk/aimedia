//! Safe, runtime-loaded boundary for the pinned Android libxaac build.
//!
//! The native shim owns libxaac's allocator/configuration command sequences. Rust owns the opaque
//! handles, validates the fixed Alpha ADTS profile, and exposes interleaved `f32` PCM.

use std::{collections::VecDeque, ffi::c_void, ptr::NonNull, sync::Arc};

use aimedia_core::{
    Timestamp,
    backend::{
        AudioDecoder as CoreAudioDecoder, AudioEncoder as CoreAudioEncoder, AudioFrame,
        BackendError, CodecId, MediaPacket,
    },
};
use aimedia_mpegts::elementary::parse_adts_frame;
use async_trait::async_trait;
use bytes::Bytes;
use libloading::Library;
use serde::Serialize;
use thiserror::Error;

pub const SAMPLE_RATE_HZ: u32 = 48_000;
pub const CHANNELS: u8 = 2;
pub const BITRATE_KBPS: u32 = 128;
pub const SAMPLES_PER_CHANNEL: usize = 1_024;
pub const INTERLEAVED_SAMPLES_PER_FRAME: usize = SAMPLES_PER_CHANNEL * CHANNELS as usize;

const ADTS_OUTPUT_CAPACITY: usize = 8_191;
const MAX_PCM_BATCH_SAMPLES: usize = INTERLEAVED_SAMPLES_PER_FRAME * 32;

const STATUS_OK: i32 = 0;
const STATUS_NEED_MORE_INPUT: i32 = 1;
const STATUS_OUT_OF_MEMORY: i32 = -2;
const STATUS_INITIALIZATION_ERROR: i32 = -3;
const STATUS_UNSUPPORTED_FORMAT: i32 = -4;
const STATUS_CORRUPT_INPUT: i32 = -5;
const STATUS_OUTPUT_TOO_SMALL: i32 = -7;
const STATUS_INPUT_LIMIT_EXCEEDED: i32 = -8;

type SymbolMarker = unsafe extern "C" fn();
type DecoderCreate = unsafe extern "C" fn(*mut *mut c_void, *mut i32) -> i32;
type DecoderDecode = unsafe extern "C" fn(
    *mut c_void,
    *const u8,
    usize,
    *mut f32,
    usize,
    *mut usize,
    *mut i32,
) -> i32;
type DecoderFlush = unsafe extern "C" fn(*mut c_void, *mut f32, usize, *mut usize, *mut i32) -> i32;
type DecoderDestroy = unsafe extern "C" fn(*mut c_void);
type EncoderCreate = unsafe extern "C" fn(*mut *mut c_void, *mut i32) -> i32;
type EncoderEncode = unsafe extern "C" fn(
    *mut c_void,
    *const f32,
    usize,
    *mut u8,
    usize,
    *mut usize,
    *mut i32,
) -> i32;
type EncoderDestroy = unsafe extern "C" fn(*mut c_void);

#[derive(Debug, Error)]
pub enum AacError {
    #[error("could not load {library}: {message}")]
    Library {
        library: &'static str,
        message: String,
    },
    #[error("libxaac symbol {symbol} is unavailable: {message}")]
    Symbol {
        symbol: &'static str,
        message: String,
    },
    #[error("AAC alpha profile requires AAC-LC, 48000 Hz, two channels, and 128 kbps")]
    UnsupportedProfile,
    #[error("invalid AAC-LC ADTS frame: {0}")]
    InvalidAdts(String),
    #[error(
        "libxaac initialization failed during {operation}: bridge status {status}, native code 0x{native_code:08x}"
    )]
    Initialization {
        operation: &'static str,
        status: i32,
        native_code: u32,
    },
    #[error(
        "libxaac rejected the configured format during {operation}: bridge status {status}, native code 0x{native_code:08x}"
    )]
    Format {
        operation: &'static str,
        status: i32,
        native_code: u32,
    },
    #[error(
        "libxaac reported corrupt data during {operation}: bridge status {status}, native code 0x{native_code:08x}"
    )]
    CorruptData {
        operation: &'static str,
        status: i32,
        native_code: u32,
    },
    #[error(
        "libxaac processing failed during {operation}: bridge status {status}, native code 0x{native_code:08x}"
    )]
    Processing {
        operation: &'static str,
        status: i32,
        native_code: u32,
    },
    #[error("AAC input batch exceeds the hard limit of {limit_samples} interleaved samples")]
    InputLimit { limit_samples: usize },
    #[error("compressed AAC buffering exceeds the hard limit of {limit_bytes} bytes")]
    CompressedInputLimit { limit_bytes: usize },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AacProbeReport {
    pub decoder_library: &'static str,
    pub encoder_library: &'static str,
    pub bridge_library: &'static str,
    pub decoder_symbol: &'static str,
    pub encoder_symbols: [&'static str; 3],
    pub frame_processing: bool,
    pub profile: &'static str,
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate_kbps: u32,
}

#[derive(Clone, Copy)]
struct NativeFunctions {
    decoder_create: DecoderCreate,
    decoder_decode: DecoderDecode,
    decoder_flush: DecoderFlush,
    decoder_destroy: DecoderDestroy,
    encoder_create: EncoderCreate,
    encoder_encode: EncoderEncode,
    encoder_destroy: EncoderDestroy,
}

pub struct Libxaac {
    _decoder_library: Library,
    _encoder_library: Library,
    _bridge_library: Library,
    functions: NativeFunctions,
    report: AacProbeReport,
}

// The libraries are immutable after loading and the function table contains plain C pointers.
unsafe impl Send for Libxaac {}
unsafe impl Sync for Libxaac {}

impl std::fmt::Debug for Libxaac {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Libxaac")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

impl Libxaac {
    pub fn load() -> Result<Arc<Self>, AacError> {
        let (decoder_library, decoder_name) = load_first(
            &["libxaacdec.so", "libxaacdec.so.0"],
            "Android libxaac decoder",
        )?;
        let (encoder_library, encoder_name) = load_first(
            &["libxaacenc.so", "libxaacenc.so.0"],
            "Android libxaac encoder",
        )?;
        let (bridge_library, bridge_name) = load_first(
            &["libaimedia_xaac.so", "libaimedia_xaac.so.0"],
            "aimedia libxaac bridge",
        )?;

        // SAFETY: marker signatures are never called; these checks verify the pinned libraries.
        unsafe { marker(&decoder_library, b"ixheaacd_dec_api\0", "ixheaacd_dec_api")? };
        // SAFETY: same symbol-existence check.
        unsafe { marker(&encoder_library, b"ixheaace_create\0", "ixheaace_create")? };
        // SAFETY: same symbol-existence check.
        unsafe { marker(&encoder_library, b"ixheaace_process\0", "ixheaace_process")? };
        // SAFETY: same symbol-existence check.
        unsafe { marker(&encoder_library, b"ixheaace_delete\0", "ixheaace_delete")? };

        // SAFETY: signatures are defined by aimedia_xaac.h, built from this source tree.
        let functions = unsafe {
            NativeFunctions {
                decoder_create: symbol(
                    &bridge_library,
                    b"aimedia_xaac_decoder_create\0",
                    "aimedia_xaac_decoder_create",
                )?,
                decoder_decode: symbol(
                    &bridge_library,
                    b"aimedia_xaac_decoder_decode\0",
                    "aimedia_xaac_decoder_decode",
                )?,
                decoder_flush: symbol(
                    &bridge_library,
                    b"aimedia_xaac_decoder_flush\0",
                    "aimedia_xaac_decoder_flush",
                )?,
                decoder_destroy: symbol(
                    &bridge_library,
                    b"aimedia_xaac_decoder_destroy\0",
                    "aimedia_xaac_decoder_destroy",
                )?,
                encoder_create: symbol(
                    &bridge_library,
                    b"aimedia_xaac_encoder_create\0",
                    "aimedia_xaac_encoder_create",
                )?,
                encoder_encode: symbol(
                    &bridge_library,
                    b"aimedia_xaac_encoder_encode\0",
                    "aimedia_xaac_encoder_encode",
                )?,
                encoder_destroy: symbol(
                    &bridge_library,
                    b"aimedia_xaac_encoder_destroy\0",
                    "aimedia_xaac_encoder_destroy",
                )?,
            }
        };

        let library = Arc::new(Self {
            _decoder_library: decoder_library,
            _encoder_library: encoder_library,
            _bridge_library: bridge_library,
            functions,
            report: AacProbeReport {
                decoder_library: decoder_name,
                encoder_library: encoder_name,
                bridge_library: bridge_name,
                decoder_symbol: "ixheaacd_dec_api",
                encoder_symbols: ["ixheaace_create", "ixheaace_process", "ixheaace_delete"],
                frame_processing: true,
                profile: "AAC-LC",
                sample_rate: SAMPLE_RATE_HZ,
                channels: CHANNELS,
                bitrate_kbps: BITRATE_KBPS,
            },
        });
        // Validate allocator/configuration paths as part of doctor, not only symbol presence.
        drop(library.decoder()?);
        drop(library.encoder()?);
        Ok(library)
    }

    pub fn validate_profile(
        sample_rate: u32,
        channels: u8,
        bitrate_kbps: u32,
    ) -> Result<(), AacError> {
        if sample_rate == SAMPLE_RATE_HZ && channels == CHANNELS && bitrate_kbps == BITRATE_KBPS {
            Ok(())
        } else {
            Err(AacError::UnsupportedProfile)
        }
    }

    pub fn decoder(self: &Arc<Self>) -> Result<AacDecoder, AacError> {
        AacDecoder::new(Arc::clone(self))
    }

    pub fn encoder(self: &Arc<Self>) -> Result<AacEncoder, AacError> {
        AacEncoder::new(Arc::clone(self))
    }

    pub fn audio_decoder(self: &Arc<Self>) -> Result<LibxaacAudioDecoder, AacError> {
        Ok(LibxaacAudioDecoder {
            inner: self.decoder()?,
            timeline: SampleTimeline::default(),
        })
    }

    pub fn audio_encoder(self: &Arc<Self>) -> Result<LibxaacAudioEncoder, AacError> {
        Ok(LibxaacAudioEncoder {
            inner: self.encoder()?,
            timeline: SampleTimeline::default(),
        })
    }

    #[must_use]
    pub fn report(&self) -> &AacProbeReport {
        &self.report
    }
}

#[derive(Debug)]
pub struct DecodedPcm {
    pub sample_rate: u32,
    pub channels: u8,
    pub interleaved: Vec<f32>,
}

pub struct AacDecoder {
    library: Arc<Libxaac>,
    handle: NonNull<c_void>,
    pending_pcm: VecDeque<f32>,
    native_flushed: bool,
}

// A decoder is single-owner and libxaac does not retain thread-local pointers between calls.
unsafe impl Send for AacDecoder {}

impl std::fmt::Debug for AacDecoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AacDecoder")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl AacDecoder {
    fn new(library: Arc<Libxaac>) -> Result<Self, AacError> {
        let mut handle = std::ptr::null_mut();
        let mut native_code = 0;
        // SAFETY: outputs point to initialized stack storage and the library outlives the handle.
        let status = unsafe { (library.functions.decoder_create)(&mut handle, &mut native_code) };
        let handle = NonNull::new(handle)
            .filter(|_| status == STATUS_OK)
            .ok_or_else(|| native_error("decoderCreate", status, native_code))?;
        Ok(Self {
            library,
            handle,
            pending_pcm: VecDeque::with_capacity(INTERLEAVED_SAMPLES_PER_FRAME * 2),
            native_flushed: false,
        })
    }

    pub fn decode_adts(&mut self, adts: &[u8]) -> Result<Option<DecodedPcm>, AacError> {
        validate_adts(adts)?;
        let mut pcm = vec![0.0_f32; INTERLEAVED_SAMPLES_PER_FRAME];
        let mut pcm_samples = 0_usize;
        let mut native_code = 0;
        // SAFETY: the handle is exclusively borrowed; all slices are valid for their lengths.
        let status = unsafe {
            (self.library.functions.decoder_decode)(
                self.handle.as_ptr(),
                adts.as_ptr(),
                adts.len(),
                pcm.as_mut_ptr(),
                pcm.len(),
                &mut pcm_samples,
                &mut native_code,
            )
        };
        if let Some(frame) = decode_result(status, native_code, "decode", pcm, pcm_samples)? {
            self.pending_pcm.extend(frame.interleaved);
        }
        Ok(self.take_complete_frame())
    }

    pub fn flush(&mut self) -> Result<Option<DecodedPcm>, AacError> {
        if !self.native_flushed {
            let mut pcm = vec![0.0_f32; INTERLEAVED_SAMPLES_PER_FRAME];
            let mut pcm_samples = 0_usize;
            let mut native_code = 0;
            // SAFETY: the handle is exclusively borrowed and the output slice is writable.
            let status = unsafe {
                (self.library.functions.decoder_flush)(
                    self.handle.as_ptr(),
                    pcm.as_mut_ptr(),
                    pcm.len(),
                    &mut pcm_samples,
                    &mut native_code,
                )
            };
            if let Some(frame) =
                decode_result(status, native_code, "decoderFlush", pcm, pcm_samples)?
            {
                self.pending_pcm.extend(frame.interleaved);
            }
            self.native_flushed = true;
        }
        if let Some(frame) = self.take_complete_frame() {
            return Ok(Some(frame));
        }
        if self.pending_pcm.is_empty() {
            return Ok(None);
        }
        self.pending_pcm
            .resize(INTERLEAVED_SAMPLES_PER_FRAME, 0.0_f32);
        Ok(self.take_complete_frame())
    }

    fn take_complete_frame(&mut self) -> Option<DecodedPcm> {
        if self.pending_pcm.len() < INTERLEAVED_SAMPLES_PER_FRAME {
            return None;
        }
        Some(DecodedPcm {
            sample_rate: SAMPLE_RATE_HZ,
            channels: CHANNELS,
            interleaved: self
                .pending_pcm
                .drain(..INTERLEAVED_SAMPLES_PER_FRAME)
                .collect(),
        })
    }
}

impl Drop for AacDecoder {
    fn drop(&mut self) {
        // SAFETY: this is the only owner and destroy accepts a handle created by this bridge.
        unsafe { (self.library.functions.decoder_destroy)(self.handle.as_ptr()) };
    }
}

pub struct AacEncoder {
    library: Arc<Libxaac>,
    handle: NonNull<c_void>,
    pending: VecDeque<f32>,
}

// An encoder is single-owner and libxaac does not retain thread-local pointers between calls.
unsafe impl Send for AacEncoder {}

impl std::fmt::Debug for AacEncoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AacEncoder")
            .field("handle", &self.handle)
            .field("pending_samples", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl AacEncoder {
    fn new(library: Arc<Libxaac>) -> Result<Self, AacError> {
        let mut handle = std::ptr::null_mut();
        let mut native_code = 0;
        // SAFETY: outputs point to initialized stack storage and the library outlives the handle.
        let status = unsafe { (library.functions.encoder_create)(&mut handle, &mut native_code) };
        let handle = NonNull::new(handle)
            .filter(|_| status == STATUS_OK)
            .ok_or_else(|| native_error("encoderCreate", status, native_code))?;
        Ok(Self {
            library,
            handle,
            pending: VecDeque::with_capacity(INTERLEAVED_SAMPLES_PER_FRAME),
        })
    }

    pub fn encode_interleaved(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>, AacError> {
        if pcm.len() > MAX_PCM_BATCH_SAMPLES {
            return Err(AacError::InputLimit {
                limit_samples: MAX_PCM_BATCH_SAMPLES,
            });
        }
        self.pending.extend(pcm.iter().copied());
        let mut output = Vec::new();
        while self.pending.len() >= INTERLEAVED_SAMPLES_PER_FRAME {
            let frame: Vec<f32> = self
                .pending
                .drain(..INTERLEAVED_SAMPLES_PER_FRAME)
                .collect();
            output.push(self.encode_frame(&frame)?);
        }
        Ok(output)
    }

    pub fn flush(&mut self) -> Result<Vec<Vec<u8>>, AacError> {
        if self.pending.is_empty() {
            return Ok(Vec::new());
        }
        self.pending.resize(INTERLEAVED_SAMPLES_PER_FRAME, 0.0_f32);
        let frame: Vec<f32> = self.pending.drain(..).collect();
        Ok(vec![self.encode_frame(&frame)?])
    }

    fn encode_frame(&mut self, pcm: &[f32]) -> Result<Vec<u8>, AacError> {
        let mut adts = vec![0_u8; ADTS_OUTPUT_CAPACITY];
        let mut adts_length = 0_usize;
        let mut native_code = 0;
        // SAFETY: the handle is exclusively borrowed and all slices are valid for their lengths.
        let status = unsafe {
            (self.library.functions.encoder_encode)(
                self.handle.as_ptr(),
                pcm.as_ptr(),
                pcm.len(),
                adts.as_mut_ptr(),
                adts.len(),
                &mut adts_length,
                &mut native_code,
            )
        };
        if status != STATUS_OK {
            return Err(native_error("encode", status, native_code));
        }
        if adts_length > adts.len() {
            return Err(AacError::Processing {
                operation: "encodeLength",
                status: STATUS_OUTPUT_TOO_SMALL,
                native_code: native_code as u32,
            });
        }
        adts.truncate(adts_length);
        validate_adts(&adts)?;
        Ok(adts)
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        // SAFETY: this is the only owner and destroy accepts a handle created by this bridge.
        unsafe { (self.library.functions.encoder_destroy)(self.handle.as_ptr()) };
    }
}

/// `aimedia-core` decoder adapter around the fixed-profile libxaac decoder.
#[derive(Debug)]
pub struct LibxaacAudioDecoder {
    inner: AacDecoder,
    timeline: SampleTimeline,
}

#[async_trait]
impl CoreAudioDecoder for LibxaacAudioDecoder {
    async fn decode(&mut self, packet: MediaPacket) -> Result<Vec<AudioFrame>, BackendError> {
        if packet.codec != CodecId::AacLc {
            return Err(BackendError::Unsupported(format!(
                "libxaac decoder requires AAC-LC packets, got {:?}",
                packet.codec
            )));
        }
        if packet.discontinuity {
            self.inner =
                AacDecoder::new(Arc::clone(&self.inner.library)).map_err(map_backend_error)?;
            self.timeline.reset();
        }
        self.timeline.anchor(packet.pts);
        let decoded = self
            .inner
            .decode_adts(&packet.data)
            .map_err(map_backend_error)?;
        decoded
            .map(|frame| self.audio_frame(frame))
            .transpose()
            .map(|frame| frame.into_iter().collect())
    }

    async fn flush(&mut self) -> Result<Vec<AudioFrame>, BackendError> {
        let mut frames = Vec::new();
        while let Some(frame) = self.inner.flush().map_err(map_backend_error)? {
            frames.push(self.audio_frame(frame)?);
        }
        Ok(frames)
    }
}

impl LibxaacAudioDecoder {
    fn audio_frame(&mut self, frame: DecodedPcm) -> Result<AudioFrame, BackendError> {
        Ok(AudioFrame {
            pts: self.timeline.next(SAMPLES_PER_CHANNEL)?,
            sample_rate: frame.sample_rate,
            channels: frame.channels,
            interleaved: frame.interleaved,
        })
    }
}

/// `aimedia-core` encoder adapter around the fixed-profile libxaac encoder.
#[derive(Debug)]
pub struct LibxaacAudioEncoder {
    inner: AacEncoder,
    timeline: SampleTimeline,
}

#[async_trait]
impl CoreAudioEncoder for LibxaacAudioEncoder {
    async fn encode(&mut self, frame: AudioFrame) -> Result<Vec<MediaPacket>, BackendError> {
        if frame.sample_rate != SAMPLE_RATE_HZ
            || frame.channels != CHANNELS
            || frame.interleaved.len() % usize::from(CHANNELS) != 0
        {
            return Err(BackendError::Unsupported(
                "libxaac encoder requires interleaved f32 PCM at 48000 Hz with two channels"
                    .to_owned(),
            ));
        }
        self.timeline.anchor(frame.pts);
        let packets = self
            .inner
            .encode_interleaved(&frame.interleaved)
            .map_err(map_backend_error)?;
        self.media_packets(packets)
    }

    async fn flush(&mut self) -> Result<Vec<MediaPacket>, BackendError> {
        let packets = self.inner.flush().map_err(map_backend_error)?;
        self.media_packets(packets)
    }
}

impl LibxaacAudioEncoder {
    fn media_packets(&mut self, packets: Vec<Vec<u8>>) -> Result<Vec<MediaPacket>, BackendError> {
        packets
            .into_iter()
            .map(|data| {
                let pts = self.timeline.next(SAMPLES_PER_CHANNEL)?;
                Ok(MediaPacket {
                    stream_id: 0,
                    codec: CodecId::AacLc,
                    pts,
                    dts: Some(pts),
                    duration: Some(sample_duration(pts.timescale, SAMPLES_PER_CHANNEL)),
                    keyframe: true,
                    discontinuity: false,
                    data: Bytes::from(data),
                })
            })
            .collect()
    }
}

#[derive(Debug, Default)]
struct SampleTimeline {
    anchor: Option<Timestamp>,
    emitted_samples: u64,
}

impl SampleTimeline {
    fn anchor(&mut self, timestamp: Timestamp) {
        self.anchor.get_or_insert(timestamp);
    }

    fn next(&mut self, samples: usize) -> Result<Timestamp, BackendError> {
        let anchor = self.anchor.ok_or_else(|| {
            BackendError::Processing("AAC output was produced before a timestamp anchor".to_owned())
        })?;
        let offset = i128::from(self.emitted_samples) * i128::from(anchor.timescale)
            / i128::from(SAMPLE_RATE_HZ);
        self.emitted_samples = self
            .emitted_samples
            .saturating_add(u64::try_from(samples).unwrap_or(u64::MAX));
        Ok(Timestamp::new(
            (i128::from(anchor.ticks) + offset).clamp(i128::from(i64::MIN), i128::from(i64::MAX))
                as i64,
            anchor.timescale,
        ))
    }

    fn reset(&mut self) {
        self.anchor = None;
        self.emitted_samples = 0;
    }
}

fn sample_duration(timescale: u32, samples: usize) -> Timestamp {
    let ticks = u128::try_from(samples)
        .unwrap_or(u128::MAX)
        .saturating_mul(u128::from(timescale))
        / u128::from(SAMPLE_RATE_HZ);
    Timestamp::new(
        i64::try_from(ticks.min(i128::from(i64::MAX) as u128)).unwrap_or(i64::MAX),
        timescale,
    )
}

fn map_backend_error(error: AacError) -> BackendError {
    let message = error.to_string();
    match error {
        AacError::Library { .. } | AacError::Symbol { .. } | AacError::Initialization { .. } => {
            BackendError::Unavailable(message)
        }
        AacError::UnsupportedProfile | AacError::Format { .. } => {
            BackendError::Unsupported(message)
        }
        AacError::InvalidAdts(_)
        | AacError::CorruptData { .. }
        | AacError::Processing { .. }
        | AacError::InputLimit { .. }
        | AacError::CompressedInputLimit { .. } => BackendError::Processing(message),
    }
}

fn decode_result(
    status: i32,
    native_code: i32,
    operation: &'static str,
    mut pcm: Vec<f32>,
    pcm_samples: usize,
) -> Result<Option<DecodedPcm>, AacError> {
    if status == STATUS_NEED_MORE_INPUT {
        return Ok(None);
    }
    if status != STATUS_OK {
        return Err(native_error(operation, status, native_code));
    }
    if pcm_samples > pcm.len() || pcm_samples % CHANNELS as usize != 0 {
        return Err(AacError::Processing {
            operation: "decodeLength",
            status: STATUS_OUTPUT_TOO_SMALL,
            native_code: native_code as u32,
        });
    }
    pcm.truncate(pcm_samples);
    Ok(Some(DecodedPcm {
        sample_rate: SAMPLE_RATE_HZ,
        channels: CHANNELS,
        interleaved: pcm,
    }))
}

fn validate_adts(adts: &[u8]) -> Result<(), AacError> {
    let frame = parse_adts_frame(adts).map_err(|error| AacError::InvalidAdts(error.to_string()))?;
    if frame.bytes.len() != adts.len() {
        return Err(AacError::InvalidAdts(
            "one call must contain exactly one ADTS frame".to_owned(),
        ));
    }
    if frame.header.audio_object_type != 2
        || frame.header.sample_rate_hz != SAMPLE_RATE_HZ
        || frame.header.channel_configuration != CHANNELS
        || frame.header.raw_data_blocks != 0
    {
        return Err(AacError::UnsupportedProfile);
    }
    Ok(())
}

fn native_error(operation: &'static str, status: i32, native_code: i32) -> AacError {
    let native_code = native_code as u32;
    match status {
        STATUS_OUT_OF_MEMORY | STATUS_INITIALIZATION_ERROR => AacError::Initialization {
            operation,
            status,
            native_code,
        },
        STATUS_UNSUPPORTED_FORMAT => AacError::Format {
            operation,
            status,
            native_code,
        },
        STATUS_CORRUPT_INPUT => AacError::CorruptData {
            operation,
            status,
            native_code,
        },
        STATUS_INPUT_LIMIT_EXCEEDED => AacError::CompressedInputLimit {
            limit_bytes: 64 * 1_024,
        },
        _ => AacError::Processing {
            operation,
            status,
            native_code,
        },
    }
}

fn load_first(
    candidates: &[&'static str],
    display_name: &'static str,
) -> Result<(Library, &'static str), AacError> {
    let mut last_error = String::new();
    for candidate in candidates {
        // SAFETY: the returned owner remains alive for every resolved symbol.
        match unsafe { Library::new(candidate) } {
            Ok(library) => return Ok((library, candidate)),
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(AacError::Library {
        library: display_name,
        message: last_error,
    })
}

unsafe fn marker(
    library: &Library,
    name: &[u8],
    display_name: &'static str,
) -> Result<(), AacError> {
    // SAFETY: the marker is never invoked.
    unsafe { library.get::<SymbolMarker>(name) }
        .map(|_| ())
        .map_err(|error| AacError::Symbol {
            symbol: display_name,
            message: error.to_string(),
        })
}

unsafe fn symbol<T: Copy>(
    library: &Library,
    name: &[u8],
    display_name: &'static str,
) -> Result<T, AacError> {
    // SAFETY: caller supplies the signature from aimedia_xaac.h and keeps the library alive.
    unsafe { library.get::<T>(name) }
        .map(|value| *value)
        .map_err(|error| AacError::Symbol {
            symbol: display_name,
            message: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use aimedia_core::{
        Timestamp,
        backend::{AudioDecoder as CoreAudioDecoder, AudioEncoder as CoreAudioEncoder, AudioFrame},
    };

    use super::{
        AacError, INTERLEAVED_SAMPLES_PER_FRAME, Libxaac, SAMPLE_RATE_HZ, SAMPLES_PER_CHANNEL,
        SampleTimeline, validate_adts,
    };

    #[test]
    fn validates_the_only_alpha_audio_profile() {
        Libxaac::validate_profile(48_000, 2, 128).expect("alpha profile is accepted");
        assert!(matches!(
            Libxaac::validate_profile(44_100, 2, 128),
            Err(AacError::UnsupportedProfile)
        ));
    }

    #[test]
    fn rejects_damaged_adts_before_native_decode() {
        let damaged = [0xff, 0xf1, 0x4c, 0x80, 0x7f, 0xff, 0xfc];
        assert!(matches!(
            validate_adts(&damaged),
            Err(AacError::InvalidAdts(_))
        ));
    }

    #[test]
    fn sample_timeline_has_exact_1024_sample_cadence() {
        let mut timeline = SampleTimeline::default();
        timeline.anchor(Timestamp::new(90_000, 90_000));
        let points: Vec<i64> = (0..1_001)
            .map(|_| {
                timeline
                    .next(SAMPLES_PER_CHANNEL)
                    .expect("timeline has an anchor")
                    .ticks
            })
            .collect();
        assert_eq!(points[0], 90_000);
        assert_eq!(points[1], 91_920);
        assert_eq!(points[1_000], 2_010_000);
        assert!(points.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    #[ignore = "requires the pinned libxaac libraries and aimedia native bridge"]
    fn native_round_trip_uses_1024_sample_cadence() {
        let library = Libxaac::load().expect("load native codec");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build test runtime");
        runtime.block_on(async move {
            let mut encoder = library.audio_encoder().expect("create encoder adapter");
            let mut decoder = library.audio_decoder().expect("create decoder adapter");
            let mut encoded = Vec::new();

            for frame_index in 0..4 {
                let mut pcm = Vec::with_capacity(INTERLEAVED_SAMPLES_PER_FRAME);
                for sample in 0..SAMPLES_PER_CHANNEL {
                    let phase = ((frame_index * SAMPLES_PER_CHANNEL + sample) as f32
                        * 440.0
                        * std::f32::consts::TAU
                        / SAMPLE_RATE_HZ as f32)
                        .sin()
                        * 0.2;
                    pcm.extend([phase, phase]);
                }
                encoded.extend(
                    CoreAudioEncoder::encode(
                        &mut encoder,
                        AudioFrame {
                            pts: Timestamp::new(90_000 + frame_index as i64 * 1_920, 90_000),
                            sample_rate: SAMPLE_RATE_HZ,
                            channels: 2,
                            interleaved: pcm,
                        },
                    )
                    .await
                    .expect("encode one PCM frame"),
                );
            }
            encoded.extend(
                CoreAudioEncoder::flush(&mut encoder)
                    .await
                    .expect("flush encoder"),
            );
            assert!(!encoded.is_empty(), "no encoded packets");
            assert_eq!(encoded[0].pts, Timestamp::new(90_000, 90_000));
            assert!(encoded.windows(2).all(|pair| pair[0].pts < pair[1].pts));
            assert!(
                encoded
                    .iter()
                    .all(|packet| { packet.duration == Some(Timestamp::new(1_920, 90_000)) })
            );

            let mut decoded = Vec::new();
            for packet in encoded {
                decoded.extend(
                    CoreAudioDecoder::decode(&mut decoder, packet)
                        .await
                        .expect("decode ADTS packet"),
                );
            }
            decoded.extend(
                CoreAudioDecoder::flush(&mut decoder)
                    .await
                    .expect("flush decoder"),
            );

            let decoded_shapes: Vec<_> = decoded
                .iter()
                .map(|frame| (frame.interleaved.len(), frame.sample_rate, frame.channels))
                .collect();
            assert!(!decoded.is_empty(), "no decoded frames");
            assert!(
                decoded.iter().all(|frame| {
                    frame.interleaved.len() == INTERLEAVED_SAMPLES_PER_FRAME
                        && frame.sample_rate == SAMPLE_RATE_HZ
                        && frame.channels == 2
                }),
                "decoded frame shapes: {decoded_shapes:?}"
            );
            assert!(decoded.windows(2).all(|pair| pair[0].pts < pair[1].pts));
        });
    }
}

//! Runtime availability boundary for Android libxaac.
//!
//! libxaac's encoder and decoder require allocator/configuration command sequences. This crate
//! validates the pinned symbols without forcing CPU-only builds to link the native library.

use std::sync::Arc;

use libloading::Library;
use serde::Serialize;
use thiserror::Error;

type SymbolMarker = unsafe extern "C" fn();

#[derive(Debug, Error)]
pub enum AacError {
    #[error("could not load Android libxaac: {0}")]
    Library(String),
    #[error("libxaac symbol {symbol} is unavailable: {message}")]
    Symbol {
        symbol: &'static str,
        message: String,
    },
    #[error("AAC alpha profile requires AAC-LC, 48000 Hz, two channels, and 128 kbps")]
    UnsupportedProfile,
    #[error(
        "libxaac frame processing is not enabled yet; native symbols passed availability checks"
    )]
    ProcessingNotImplemented,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AacProbeReport {
    pub decoder_library: &'static str,
    pub encoder_library: &'static str,
    pub decoder_symbol: &'static str,
    pub encoder_symbols: [&'static str; 3],
    pub profile: &'static str,
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate_kbps: u32,
}

pub struct Libxaac {
    _decoder_library: Library,
    _encoder_library: Library,
    report: AacProbeReport,
}

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
        let (decoder_library, decoder_name) = load_first(&["libxaacdec.so", "libxaacdec.so.0"])?;
        let (encoder_library, encoder_name) = load_first(&["libxaacenc.so", "libxaacenc.so.0"])?;
        // SAFETY: this only confirms the symbol; it is not invoked through the marker signature.
        unsafe { symbol(&decoder_library, b"ixheaacd_dec_api\0", "ixheaacd_dec_api")? };
        // SAFETY: same symbol-existence check.
        unsafe { symbol(&encoder_library, b"ixheaace_create\0", "ixheaace_create")? };
        // SAFETY: same symbol-existence check.
        unsafe { symbol(&encoder_library, b"ixheaace_process\0", "ixheaace_process")? };
        // SAFETY: same symbol-existence check.
        unsafe { symbol(&encoder_library, b"ixheaace_delete\0", "ixheaace_delete")? };
        Ok(Arc::new(Self {
            _decoder_library: decoder_library,
            _encoder_library: encoder_library,
            report: AacProbeReport {
                decoder_library: decoder_name,
                encoder_library: encoder_name,
                decoder_symbol: "ixheaacd_dec_api",
                encoder_symbols: ["ixheaace_create", "ixheaace_process", "ixheaace_delete"],
                profile: "AAC-LC",
                sample_rate: 48_000,
                channels: 2,
                bitrate_kbps: 128,
            },
        }))
    }

    pub fn validate_profile(
        sample_rate: u32,
        channels: u8,
        bitrate_kbps: u32,
    ) -> Result<(), AacError> {
        if sample_rate == 48_000 && channels == 2 && bitrate_kbps == 128 {
            Ok(())
        } else {
            Err(AacError::UnsupportedProfile)
        }
    }

    #[must_use]
    pub fn report(&self) -> &AacProbeReport {
        &self.report
    }
}

fn load_first(candidates: &[&'static str]) -> Result<(Library, &'static str), AacError> {
    let mut last_error = String::new();
    for candidate in candidates {
        // SAFETY: the returned owner keeps the library loaded while symbols are inspected.
        match unsafe { Library::new(candidate) } {
            Ok(library) => return Ok((library, candidate)),
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(AacError::Library(last_error))
}

unsafe fn symbol(
    library: &Library,
    name: &[u8],
    display_name: &'static str,
) -> Result<(), AacError> {
    // SAFETY: the marker is never called; this only confirms that the public function symbol
    // exists. Frame processing will introduce exact generated bindings from the pinned headers.
    unsafe { library.get::<SymbolMarker>(name) }
        .map(|_| ())
        .map_err(|error| AacError::Symbol {
            symbol: display_name,
            message: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::{AacError, Libxaac};

    #[test]
    fn validates_the_only_alpha_audio_profile() {
        Libxaac::validate_profile(48_000, 2, 128).expect("alpha profile is accepted");
        assert!(matches!(
            Libxaac::validate_profile(44_100, 2, 128),
            Err(AacError::UnsupportedProfile)
        ));
    }
}

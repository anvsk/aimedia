//! NVIDIA Video Codec SDK 13.0 loading and resource ownership.
//!
//! Decoder/encoder frame submission is intentionally kept behind this crate. CPU-only builds do
//! not link NVIDIA libraries; availability is checked at runtime.

use std::{
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
};

use aimedia_core::backend::{SurfaceLease, VideoSurface};
use libloading::Library;
use serde::Serialize;
use thiserror::Error;

pub const VIDEO_CODEC_SDK_MAJOR: u32 = 13;
pub const VIDEO_CODEC_SDK_MINOR: u32 = 0;
const REQUIRED_NVENC_API_VERSION_RAW: u32 = (VIDEO_CODEC_SDK_MAJOR << 4) | VIDEO_CODEC_SDK_MINOR;

type CuInit = unsafe extern "C" fn(u32) -> i32;
type CuDriverGetVersion = unsafe extern "C" fn(*mut i32) -> i32;
type NvEncodeApiGetMaxSupportedVersion = unsafe extern "C" fn(*mut u32) -> i32;

#[derive(Debug, Error)]
pub enum NvidiaError {
    #[error("could not load {library}: {message}")]
    Library {
        library: &'static str,
        message: String,
    },
    #[error("NVIDIA symbol {symbol} is unavailable: {message}")]
    Symbol {
        symbol: &'static str,
        message: String,
    },
    #[error("NVIDIA operation {operation} failed with code {code}")]
    Operation { operation: &'static str, code: i32 },
    #[error("NVENC API reported version 0x{actual:08x}, below SDK 13.0 requirement")]
    UnsupportedNvencApi { actual: u32 },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NvidiaProbeReport {
    pub target_sdk_version: String,
    pub cuda_driver_version: i32,
    pub nvenc_max_api_version_raw: u32,
    pub cuda_library: &'static str,
    pub nvcuvid_library: &'static str,
    pub nvenc_library: &'static str,
}

pub struct NvidiaLibraries {
    _cuda: Library,
    _nvcuvid: Library,
    _nvenc: Library,
    report: NvidiaProbeReport,
}

unsafe impl Send for NvidiaLibraries {}
unsafe impl Sync for NvidiaLibraries {}

impl std::fmt::Debug for NvidiaLibraries {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NvidiaLibraries")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

impl NvidiaLibraries {
    pub fn load() -> Result<Arc<Self>, NvidiaError> {
        // SAFETY: all loaded libraries are pinned by this owning object and symbols are called with
        // signatures from CUDA/Video Codec SDK 13.0 headers.
        let (cuda, cuda_name) =
            unsafe { load_first(&["libcuda.so.1", "libcuda.so"], "CUDA driver library") }?;
        // SAFETY: same ownership rule as above.
        let (nvcuvid, nvcuvid_name) = unsafe {
            load_first(
                &["libnvcuvid.so.1", "libnvcuvid.so"],
                "NVDEC driver library",
            )
        }?;
        // SAFETY: same ownership rule as above.
        let (nvenc, nvenc_name) = unsafe {
            load_first(
                &["libnvidia-encode.so.1", "libnvidia-encode.so"],
                "NVENC driver library",
            )
        }?;

        // SAFETY: symbol signatures come from the SDK 13.0 headers.
        let cu_init: CuInit = unsafe { symbol(&cuda, b"cuInit\0", "cuInit")? };
        // SAFETY: symbol signatures come from the SDK 13.0 headers.
        let cu_driver_get_version: CuDriverGetVersion =
            unsafe { symbol(&cuda, b"cuDriverGetVersion\0", "cuDriverGetVersion")? };
        // SAFETY: symbol signatures come from the SDK 13.0 headers.
        let nvenc_get_version: NvEncodeApiGetMaxSupportedVersion = unsafe {
            symbol(
                &nvenc,
                b"NvEncodeAPIGetMaxSupportedVersion\0",
                "NvEncodeAPIGetMaxSupportedVersion",
            )?
        };

        // SAFETY: CUDA driver has been loaded and cuInit takes no external pointers.
        let result = unsafe { cu_init(0) };
        if result != 0 {
            return Err(NvidiaError::Operation {
                operation: "cuInit",
                code: result,
            });
        }
        let mut driver_version = 0;
        // SAFETY: output pointer is valid for one i32.
        let result = unsafe { cu_driver_get_version(&mut driver_version) };
        if result != 0 {
            return Err(NvidiaError::Operation {
                operation: "cuDriverGetVersion",
                code: result,
            });
        }
        let mut nvenc_version = 0_u32;
        // SAFETY: output pointer is valid for one u32.
        let result = unsafe { nvenc_get_version(&mut nvenc_version) };
        if result != 0 {
            return Err(NvidiaError::Operation {
                operation: "NvEncodeAPIGetMaxSupportedVersion",
                code: result,
            });
        }
        if nvenc_version < REQUIRED_NVENC_API_VERSION_RAW {
            return Err(NvidiaError::UnsupportedNvencApi {
                actual: nvenc_version,
            });
        }

        Ok(Arc::new(Self {
            _cuda: cuda,
            _nvcuvid: nvcuvid,
            _nvenc: nvenc,
            report: NvidiaProbeReport {
                target_sdk_version: format!("{VIDEO_CODEC_SDK_MAJOR}.{VIDEO_CODEC_SDK_MINOR}"),
                cuda_driver_version: driver_version,
                nvenc_max_api_version_raw: nvenc_version,
                cuda_library: cuda_name,
                nvcuvid_library: nvcuvid_name,
                nvenc_library: nvenc_name,
            },
        }))
    }

    #[must_use]
    pub fn report(&self) -> &NvidiaProbeReport {
        &self.report
    }
}

unsafe fn load_first(
    candidates: &[&'static str],
    display_name: &'static str,
) -> Result<(Library, &'static str), NvidiaError> {
    let mut last_error = String::new();
    for candidate in candidates {
        // SAFETY: caller keeps the returned library alive for every resolved symbol.
        match unsafe { Library::new(candidate) } {
            Ok(library) => return Ok((library, candidate)),
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(NvidiaError::Library {
        library: display_name,
        message: last_error,
    })
}

unsafe fn symbol<T: Copy>(
    library: &Library,
    name: &[u8],
    display_name: &'static str,
) -> Result<T, NvidiaError> {
    // SAFETY: caller supplies the exact symbol signature and owns the library.
    unsafe { library.get::<T>(name) }
        .map(|value| *value)
        .map_err(|error| NvidiaError::Symbol {
            symbol: display_name,
            message: error.to_string(),
        })
}

/// Owns one opaque CUDA/NVDEC surface and invokes its backend release callback exactly once.
pub struct CudaSurfaceLease {
    handle: u64,
    released: AtomicBool,
    release: Arc<dyn Fn(u64) + Send + Sync>,
}

impl std::fmt::Debug for CudaSurfaceLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CudaSurfaceLease")
            .field("handle", &format_args!("0x{:x}", self.handle))
            .finish_non_exhaustive()
    }
}

impl CudaSurfaceLease {
    #[must_use]
    pub fn into_video_surface(
        handle: u64,
        release: impl Fn(u64) + Send + Sync + 'static,
    ) -> VideoSurface {
        VideoSurface::new(Self {
            handle,
            released: AtomicBool::new(false),
            release: Arc::new(release),
        })
    }
}

impl SurfaceLease for CudaSurfaceLease {
    fn handle(&self) -> u64 {
        self.handle
    }
}

impl Drop for CudaSurfaceLease {
    fn drop(&mut self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            (self.release)(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::CudaSurfaceLease;

    #[test]
    fn cloned_video_surface_releases_backend_handle_once() {
        let releases = Arc::new(AtomicUsize::new(0));
        let release_counter = Arc::clone(&releases);
        let surface = CudaSurfaceLease::into_video_surface(42, move |handle| {
            assert_eq!(handle, 42);
            release_counter.fetch_add(1, Ordering::SeqCst);
        });
        let clone = surface.clone();
        drop(surface);
        assert_eq!(releases.load(Ordering::SeqCst), 0);
        drop(clone);
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }
}

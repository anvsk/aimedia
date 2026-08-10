use std::any::Any;

#[cfg(not(feature = "video-codec-sdk"))]
use aimedia_core::backend::{BackendError, VideoDecoder};
use aimedia_core::backend::{CodecId, SurfaceLease};
#[cfg(not(feature = "video-codec-sdk"))]
use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvdecCodec {
    H264,
    Hevc,
}

impl NvdecCodec {
    #[must_use]
    pub const fn packet_codec(self) -> CodecId {
        match self {
            Self::H264 => CodecId::H264,
            Self::Hevc => CodecId::H265,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvdecConfig {
    pub codec: NvdecCodec,
    pub device: u32,
    pub max_coded_width: u32,
    pub max_coded_height: u32,
    pub max_display_width: u32,
    pub max_display_height: u32,
    pub max_fps: u32,
    pub output_surfaces: u32,
    pub command_capacity: usize,
}

impl Default for NvdecConfig {
    fn default() -> Self {
        Self {
            codec: NvdecCodec::H264,
            device: 0,
            max_coded_width: 1_920,
            max_coded_height: 1_088,
            max_display_width: 1_920,
            max_display_height: 1_080,
            max_fps: 30,
            output_surfaces: 4,
            command_capacity: 16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvdecFormat {
    pub coded_width: u32,
    pub coded_height: u32,
    pub display_width: u32,
    pub display_height: u32,
    pub fps_numerator: u32,
    pub fps_denominator: u32,
    pub decode_surfaces: u32,
}

pub struct NvdecSurfaceLease {
    device_ptr: u64,
    pitch: u32,
    width: u32,
    height: u32,
    generation: u64,
    #[cfg(feature = "video-codec-sdk")]
    release: std::sync::mpsc::Sender<native::Release>,
}

impl std::fmt::Debug for NvdecSurfaceLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NvdecSurfaceLease")
            .field("devicePtr", &format_args!("0x{:x}", self.device_ptr))
            .field("pitch", &self.pitch)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl NvdecSurfaceLease {
    #[must_use]
    pub const fn device_ptr(&self) -> u64 {
        self.device_ptr
    }

    #[must_use]
    pub const fn pitch(&self) -> u32 {
        self.pitch
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

impl SurfaceLease for NvdecSurfaceLease {
    fn handle(&self) -> u64 {
        self.device_ptr
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(feature = "video-codec-sdk")]
impl Drop for NvdecSurfaceLease {
    fn drop(&mut self) {
        let _ = self.release.send(native::Release {
            generation: self.generation,
            device_ptr: self.device_ptr,
        });
    }
}

#[cfg(feature = "video-codec-sdk")]
pub use native::NvdecDecoder;

#[cfg(not(feature = "video-codec-sdk"))]
#[derive(Debug)]
pub struct NvdecDecoder;

#[cfg(not(feature = "video-codec-sdk"))]
impl NvdecDecoder {
    pub fn new(_config: NvdecConfig) -> Result<Self, BackendError> {
        Err(BackendError::Unavailable(
            "aimedia-nvidia was built without video-codec-sdk".to_owned(),
        ))
    }

    pub async fn shutdown(self) -> Result<(), BackendError> {
        Ok(())
    }
}

#[cfg(not(feature = "video-codec-sdk"))]
#[async_trait]
impl VideoDecoder for NvdecDecoder {
    async fn decode(
        &mut self,
        _packet: aimedia_core::backend::MediaPacket,
    ) -> Result<Vec<aimedia_core::backend::VideoFrame>, BackendError> {
        Err(BackendError::Unavailable(
            "aimedia-nvidia was built without video-codec-sdk".to_owned(),
        ))
    }

    async fn flush(&mut self) -> Result<Vec<aimedia_core::backend::VideoFrame>, BackendError> {
        Ok(Vec::new())
    }
}

#[cfg(feature = "video-codec-sdk")]
mod native {
    use std::{
        collections::HashMap,
        ffi::c_void,
        panic::{AssertUnwindSafe, catch_unwind},
        ptr,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender},
        },
        thread,
        time::Duration,
    };

    use aimedia_core::{
        GpuSurfaceRuntimeStats, Timestamp,
        backend::{
            BackendError, GpuSurfaceObserver, MediaPacket, MemoryDomain, PixelFormat, VideoDecoder,
            VideoFrame, VideoSurface,
        },
    };
    use async_trait::async_trait;
    use tokio::sync::oneshot;

    use super::{NvdecCodec, NvdecConfig, NvdecFormat, NvdecSurfaceLease};
    use crate::{NvidiaError, NvidiaLibraries, sdk_ffi};

    const CLOCK_RATE: u32 = 90_000;

    type CuDeviceGet = unsafe extern "C" fn(*mut sdk_ffi::CUdevice, i32) -> sdk_ffi::CUresult;
    type CuDevicePrimaryCtxRetain =
        unsafe extern "C" fn(*mut sdk_ffi::CUcontext, sdk_ffi::CUdevice) -> sdk_ffi::CUresult;
    type CuDevicePrimaryCtxRelease = unsafe extern "C" fn(sdk_ffi::CUdevice) -> sdk_ffi::CUresult;
    type CuCtxPushCurrent = unsafe extern "C" fn(sdk_ffi::CUcontext) -> sdk_ffi::CUresult;
    type CuCtxPopCurrent = unsafe extern "C" fn(*mut sdk_ffi::CUcontext) -> sdk_ffi::CUresult;
    type CuvidGetDecoderCaps =
        unsafe extern "C" fn(*mut sdk_ffi::CUVIDDECODECAPS) -> sdk_ffi::CUresult;
    type CuvidCreateDecoder = unsafe extern "C" fn(
        *mut sdk_ffi::CUvideodecoder,
        *mut sdk_ffi::CUVIDDECODECREATEINFO,
    ) -> sdk_ffi::CUresult;
    type CuvidDestroyDecoder = unsafe extern "C" fn(sdk_ffi::CUvideodecoder) -> sdk_ffi::CUresult;
    type CuvidDecodePicture = unsafe extern "C" fn(
        sdk_ffi::CUvideodecoder,
        *mut sdk_ffi::CUVIDPICPARAMS,
    ) -> sdk_ffi::CUresult;
    type CuvidMapVideoFrame = unsafe extern "C" fn(
        sdk_ffi::CUvideodecoder,
        i32,
        *mut u64,
        *mut u32,
        *mut sdk_ffi::CUVIDPROCPARAMS,
    ) -> sdk_ffi::CUresult;
    type CuvidUnmapVideoFrame =
        unsafe extern "C" fn(sdk_ffi::CUvideodecoder, u64) -> sdk_ffi::CUresult;
    type CuvidCreateParser = unsafe extern "C" fn(
        *mut sdk_ffi::CUvideoparser,
        *mut sdk_ffi::CUVIDPARSERPARAMS,
    ) -> sdk_ffi::CUresult;
    type CuvidParseVideoData = unsafe extern "C" fn(
        sdk_ffi::CUvideoparser,
        *mut sdk_ffi::CUVIDSOURCEDATAPACKET,
    ) -> sdk_ffi::CUresult;
    type CuvidDestroyParser = unsafe extern "C" fn(sdk_ffi::CUvideoparser) -> sdk_ffi::CUresult;

    #[derive(Clone, Copy)]
    struct CudaFunctions {
        device_get: CuDeviceGet,
        primary_context_retain: CuDevicePrimaryCtxRetain,
        primary_context_release: CuDevicePrimaryCtxRelease,
        context_push: CuCtxPushCurrent,
        context_pop: CuCtxPopCurrent,
    }

    #[derive(Clone, Copy)]
    struct NvdecFunctions {
        get_caps: CuvidGetDecoderCaps,
        create_decoder: CuvidCreateDecoder,
        destroy_decoder: CuvidDestroyDecoder,
        decode_picture: CuvidDecodePicture,
        map_frame: CuvidMapVideoFrame,
        unmap_frame: CuvidUnmapVideoFrame,
        create_parser: CuvidCreateParser,
        parse_data: CuvidParseVideoData,
        destroy_parser: CuvidDestroyParser,
    }

    pub struct NvdecDecoder {
        commands: SyncSender<Command>,
        worker: Option<thread::JoinHandle<()>>,
        surfaces: Arc<SurfaceMetrics>,
        codec: NvdecCodec,
        waiting_for_idr: bool,
        reset_pending: bool,
    }

    struct SurfaceMetrics {
        capacity: usize,
        in_use: AtomicUsize,
        high_watermark: AtomicUsize,
    }

    impl SurfaceMetrics {
        fn new(capacity: usize) -> Self {
            Self {
                capacity,
                in_use: AtomicUsize::new(0),
                high_watermark: AtomicUsize::new(0),
            }
        }

        fn acquire(&self) {
            let in_use = self.in_use.fetch_add(1, Ordering::AcqRel) + 1;
            self.high_watermark.fetch_max(in_use, Ordering::AcqRel);
        }

        fn release(&self) {
            let previous = self.in_use.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(previous > 0, "NVDEC surface counter underflow");
        }
    }

    impl GpuSurfaceObserver for SurfaceMetrics {
        fn stats(&self) -> GpuSurfaceRuntimeStats {
            GpuSurfaceRuntimeStats {
                in_use: self.in_use.load(Ordering::Acquire),
                capacity: self.capacity,
                high_watermark: self.high_watermark.load(Ordering::Acquire),
            }
        }
    }

    impl std::fmt::Debug for NvdecDecoder {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("NvdecDecoder")
                .field("codec", &self.codec)
                .field("waitingForIdr", &self.waiting_for_idr)
                .field("resetPending", &self.reset_pending)
                .field("surfaces", &self.surfaces.stats())
                .finish_non_exhaustive()
        }
    }

    enum Command {
        Decode {
            packet: MediaPacket,
            response: oneshot::Sender<Result<Vec<VideoFrame>, BackendError>>,
        },
        Flush {
            response: oneshot::Sender<Result<Vec<VideoFrame>, BackendError>>,
        },
        Shutdown,
    }

    pub(super) struct Release {
        pub(super) generation: u64,
        pub(super) device_ptr: u64,
    }

    impl NvdecDecoder {
        pub fn new(config: NvdecConfig) -> Result<Self, BackendError> {
            validate_config(config)?;
            let (commands, receiver) = mpsc::sync_channel(config.command_capacity);
            let (releases, release_receiver) = mpsc::channel();
            let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
            let surfaces = Arc::new(SurfaceMetrics::new(
                usize::try_from(config.output_surfaces).unwrap_or(usize::MAX),
            ));
            let worker_surfaces = Arc::clone(&surfaces);
            let worker = thread::Builder::new()
                .name(format!("aimedia-nvdec-{}", config.device))
                .spawn(move || {
                    let worker = Worker::new(config, releases, release_receiver, worker_surfaces);
                    match worker {
                        Ok(worker) => {
                            let _ = ready_sender.send(Ok(()));
                            worker.run(receiver);
                        }
                        Err(error) => {
                            let _ = ready_sender.send(Err(error));
                        }
                    }
                })
                .map_err(|error| {
                    BackendError::Unavailable(format!("could not start NVDEC worker: {error}"))
                })?;
            ready_receiver.recv().map_err(|error| {
                BackendError::Unavailable(format!("NVDEC worker exited during startup: {error}"))
            })??;
            Ok(Self {
                commands,
                worker: Some(worker),
                surfaces,
                codec: config.codec,
                waiting_for_idr: true,
                reset_pending: false,
            })
        }

        pub async fn shutdown(mut self) -> Result<(), BackendError> {
            self.commands.send(Command::Shutdown).map_err(|_| {
                BackendError::Processing("NVDEC worker command queue closed".to_owned())
            })?;
            let Some(worker) = self.worker.take() else {
                return Ok(());
            };
            tokio::task::spawn_blocking(move || worker.join())
                .await
                .map_err(|error| {
                    BackendError::Processing(format!("NVDEC shutdown task failed: {error}"))
                })?
                .map_err(|_| BackendError::Processing("NVDEC worker panicked".to_owned()))
        }

        async fn request(
            &self,
            command: impl FnOnce(oneshot::Sender<Result<Vec<VideoFrame>, BackendError>>) -> Command,
        ) -> Result<Vec<VideoFrame>, BackendError> {
            let (sender, receiver) = oneshot::channel();
            self.commands.send(command(sender)).map_err(|_| {
                BackendError::Processing("NVDEC worker command queue closed".to_owned())
            })?;
            receiver.await.map_err(|_| {
                BackendError::Processing("NVDEC worker response channel closed".to_owned())
            })?
        }
    }

    #[async_trait]
    impl VideoDecoder for NvdecDecoder {
        async fn decode(
            &mut self,
            mut packet: MediaPacket,
        ) -> Result<Vec<VideoFrame>, BackendError> {
            let expected_codec = self.codec.packet_codec();
            if packet.codec != expected_codec {
                return Err(BackendError::Unsupported(format!(
                    "NVDEC {} decoder expects {:?}, got {:?}",
                    self.codec.name(),
                    expected_codec,
                    packet.codec,
                )));
            }
            if !has_annex_b_start_code(&packet.data) {
                return Err(BackendError::Unsupported(format!(
                    "NVDEC expects an {} Annex-B access unit",
                    self.codec.name()
                )));
            }
            if packet.discontinuity {
                self.waiting_for_idr = true;
                self.reset_pending = true;
            }
            if self.waiting_for_idr && !packet.keyframe {
                return Ok(Vec::new());
            }
            if packet.keyframe {
                self.waiting_for_idr = false;
            }
            if self.reset_pending {
                packet.discontinuity = true;
            }
            let result = self
                .request(|response| Command::Decode { packet, response })
                .await;
            if result.is_ok() {
                self.reset_pending = false;
            } else {
                self.waiting_for_idr = true;
                self.reset_pending = true;
            }
            result
        }

        async fn flush(&mut self) -> Result<Vec<VideoFrame>, BackendError> {
            self.request(|response| Command::Flush { response }).await
        }

        fn surface_observer(&self) -> Option<Arc<dyn GpuSurfaceObserver>> {
            Some(self.surfaces.clone())
        }
    }

    impl Drop for NvdecDecoder {
        fn drop(&mut self) {
            let _ = self.commands.try_send(Command::Shutdown);
        }
    }

    struct Worker {
        _libraries: std::sync::Arc<NvidiaLibraries>,
        cuda: CudaFunctions,
        nvdec: NvdecFunctions,
        device: sdk_ffi::CUdevice,
        context: sdk_ffi::CUcontext,
        config: NvdecConfig,
        surfaces: Arc<SurfaceMetrics>,
        releases: Sender<Release>,
        release_receiver: Receiver<Release>,
        current: Option<Generation>,
        retired: HashMap<u64, RetiredDecoder>,
        next_generation: u64,
    }

    struct Generation {
        id: u64,
        parser: sdk_ffi::CUvideoparser,
        callback: Box<CallbackState>,
        mapped_surfaces: usize,
    }

    struct RetiredDecoder {
        decoder: sdk_ffi::CUvideodecoder,
        mapped_surfaces: usize,
    }

    struct CallbackState {
        functions: NvdecFunctions,
        config: NvdecConfig,
        decoder: sdk_ffi::CUvideodecoder,
        format: Option<NvdecFormat>,
        display_queue: Vec<sdk_ffi::CUVIDPARSERDISPINFO>,
        failure: Option<BackendError>,
    }

    impl Worker {
        fn new(
            config: NvdecConfig,
            releases: Sender<Release>,
            release_receiver: Receiver<Release>,
            surfaces: Arc<SurfaceMetrics>,
        ) -> Result<Self, BackendError> {
            let libraries = NvidiaLibraries::load().map_err(nvidia_backend_error)?;
            let cuda = load_cuda_functions(&libraries)?;
            let nvdec = load_nvdec_functions(&libraries)?;
            let mut device = 0;
            // SAFETY: output points to a valid CUdevice and the CUDA library remains loaded.
            check_cuda("cuDeviceGet", unsafe {
                (cuda.device_get)(
                    &mut device,
                    i32::try_from(config.device).map_err(|_| {
                        BackendError::Unsupported(format!(
                            "CUDA device index {} exceeds i32",
                            config.device
                        ))
                    })?,
                )
            })?;
            let mut context = ptr::null_mut();
            // Decoder and encoder workers retain the same per-device primary context. CUDA device
            // pointers are context-scoped, so this shared address space is required for the
            // GPU-to-GPU NVDEC -> NVENC copy.
            // SAFETY: device was returned by the CUDA driver and output points to a context slot.
            check_cuda("cuDevicePrimaryCtxRetain", unsafe {
                (cuda.primary_context_retain)(&mut context, device)
            })?;
            // SAFETY: the retained context is live and this worker owns its thread context stack.
            if let Err(error) = check_cuda("cuCtxPushCurrent_v2", unsafe {
                (cuda.context_push)(context)
            }) {
                // SAFETY: retain above succeeded and no CUDA resources were created.
                let _ = unsafe { (cuda.primary_context_release)(device) };
                return Err(error);
            }
            let mut worker = Self {
                _libraries: libraries,
                cuda,
                nvdec,
                device,
                context,
                config,
                surfaces,
                releases,
                release_receiver,
                current: None,
                retired: HashMap::new(),
                next_generation: 1,
            };
            if let Err(error) = worker.start_generation() {
                worker.release_context();
                return Err(error);
            }
            Ok(worker)
        }

        fn run(mut self, receiver: Receiver<Command>) {
            let mut shutting_down = false;
            loop {
                self.drain_releases();
                if shutting_down && self.retired.is_empty() {
                    break;
                }
                match receiver.recv_timeout(Duration::from_millis(5)) {
                    Ok(Command::Decode { packet, response }) => {
                        let result = if shutting_down {
                            Err(BackendError::Processing(
                                "NVDEC worker is shutting down".to_owned(),
                            ))
                        } else {
                            self.decode(packet)
                        };
                        let _ = response.send(result);
                    }
                    Ok(Command::Flush { response }) => {
                        let result = self.flush();
                        let _ = response.send(result);
                    }
                    Ok(Command::Shutdown) => {
                        if !shutting_down {
                            shutting_down = true;
                            self.retire_current();
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        if !shutting_down {
                            shutting_down = true;
                            self.retire_current();
                        }
                    }
                }
            }
        }

        // Releases stay off the bounded media queue so dropping a canceled decode response can
        // never deadlock the GPU worker. NVDEC itself caps the number of live mapped surfaces.
        fn drain_releases(&mut self) {
            while let Ok(release) = self.release_receiver.try_recv() {
                self.release_surface(release.generation, release.device_ptr);
            }
        }

        fn decode(&mut self, packet: MediaPacket) -> Result<Vec<VideoFrame>, BackendError> {
            if packet.discontinuity {
                self.retire_current();
                self.start_generation()?;
            }
            let mut source_packet = zeroed::<sdk_ffi::CUVIDSOURCEDATAPACKET>();
            source_packet.flags = (sdk_ffi::CUvideopacketflags_CUVID_PKT_TIMESTAMP
                | sdk_ffi::CUvideopacketflags_CUVID_PKT_ENDOFPICTURE)
                .into();
            source_packet.payload_size = packet.data.len().try_into().map_err(|_| {
                BackendError::Unsupported(format!(
                    "{} access unit is too large",
                    self.config.codec.name()
                ))
            })?;
            source_packet.payload = packet.data.as_ptr();
            source_packet.timestamp = to_90khz(packet.pts);
            let generation = self.current.as_mut().ok_or_else(|| {
                BackendError::Processing("NVDEC generation is unavailable".to_owned())
            })?;
            // SAFETY: parser is live; payload stays borrowed for the synchronous parser call.
            check_cuda("cuvidParseVideoData", unsafe {
                (self.nvdec.parse_data)(generation.parser, &mut source_packet)
            })?;
            if let Some(error) = generation.callback.failure.take() {
                return Err(error);
            }
            self.map_display_queue()
        }

        fn flush(&mut self) -> Result<Vec<VideoFrame>, BackendError> {
            let Some(generation) = self.current.as_mut() else {
                return Ok(Vec::new());
            };
            let mut source_packet = zeroed::<sdk_ffi::CUVIDSOURCEDATAPACKET>();
            source_packet.flags = (sdk_ffi::CUvideopacketflags_CUVID_PKT_ENDOFSTREAM
                | sdk_ffi::CUvideopacketflags_CUVID_PKT_NOTIFY_EOS)
                .into();
            // SAFETY: parser is live and EOS has no payload.
            check_cuda("cuvidParseVideoData(EOS)", unsafe {
                (self.nvdec.parse_data)(generation.parser, &mut source_packet)
            })?;
            if let Some(error) = generation.callback.failure.take() {
                return Err(error);
            }
            self.map_display_queue()
        }

        fn map_display_queue(&mut self) -> Result<Vec<VideoFrame>, BackendError> {
            let generation = self.current.as_mut().ok_or_else(|| {
                BackendError::Processing("NVDEC generation is unavailable".to_owned())
            })?;
            let displays = std::mem::take(&mut generation.callback.display_queue);
            // Parameter-only access units and parser warm-up packets legitimately produce no
            // display callback before the sequence callback has established a format.
            if displays.is_empty() {
                return Ok(Vec::new());
            }
            let format = generation.callback.format.ok_or_else(|| {
                BackendError::Processing("NVDEC parser produced a frame before format".to_owned())
            })?;
            let decoder = generation.callback.decoder;
            let mut frames = Vec::with_capacity(displays.len());
            for display in displays {
                let mut process = zeroed::<sdk_ffi::CUVIDPROCPARAMS>();
                process.progressive_frame = display.progressive_frame;
                process.top_field_first = display.top_field_first;
                process.unpaired_field = i32::from(display.repeat_first_field < 0);
                let mut device_ptr = 0_u64;
                let mut pitch = 0_u32;
                // SAFETY: decoder and picture index come from the synchronous parser callbacks.
                check_cuda("cuvidMapVideoFrame64", unsafe {
                    (self.nvdec.map_frame)(
                        decoder,
                        display.picture_index,
                        &mut device_ptr,
                        &mut pitch,
                        &mut process,
                    )
                })?;
                if device_ptr == 0 || pitch == 0 {
                    if device_ptr != 0 {
                        // SAFETY: the pointer was returned by the mapper above.
                        let _ = unsafe { (self.nvdec.unmap_frame)(decoder, device_ptr) };
                    }
                    return Err(BackendError::Processing(
                        "NVDEC returned an empty mapped surface".to_owned(),
                    ));
                }
                generation.mapped_surfaces = generation.mapped_surfaces.saturating_add(1);
                self.surfaces.acquire();
                let lease = NvdecSurfaceLease {
                    device_ptr,
                    pitch,
                    width: format.display_width,
                    height: format.display_height,
                    generation: generation.id,
                    release: self.releases.clone(),
                };
                frames.push(VideoFrame {
                    pts: Timestamp::new(display.timestamp, CLOCK_RATE),
                    width: format.display_width,
                    height: format.display_height,
                    format: PixelFormat::Nv12,
                    memory: MemoryDomain::Cuda {
                        device: self.config.device,
                    },
                    surface: VideoSurface::new(lease),
                });
            }
            Ok(frames)
        }

        fn start_generation(&mut self) -> Result<(), BackendError> {
            let id = self.next_generation;
            self.next_generation = self.next_generation.saturating_add(1);
            let mut callback = Box::new(CallbackState {
                functions: self.nvdec,
                config: self.config,
                decoder: ptr::null_mut(),
                format: None,
                display_queue: Vec::new(),
                failure: None,
            });
            let mut params = zeroed::<sdk_ffi::CUVIDPARSERPARAMS>();
            params.CodecType = sdk_codec(self.config.codec);
            params.ulMaxNumDecodeSurfaces = 1;
            params.ulClockRate = CLOCK_RATE;
            params.ulErrorThreshold = 0;
            params.ulMaxDisplayDelay = 0;
            params.pUserData = (&mut *callback as *mut CallbackState).cast::<c_void>();
            params.pfnSequenceCallback = Some(sequence_callback);
            params.pfnDecodePicture = Some(decode_callback);
            params.pfnDisplayPicture = Some(display_callback);
            let mut parser = ptr::null_mut();
            // SAFETY: parameters and callback storage remain alive for the parser lifetime.
            check_cuda("cuvidCreateVideoParser", unsafe {
                (self.nvdec.create_parser)(&mut parser, &mut params)
            })?;
            self.current = Some(Generation {
                id,
                parser,
                callback,
                mapped_surfaces: 0,
            });
            Ok(())
        }

        fn retire_current(&mut self) {
            let Some(mut generation) = self.current.take() else {
                return;
            };
            if !generation.parser.is_null() {
                // SAFETY: parser belongs to this generation and callbacks are not active here.
                let result = unsafe { (self.nvdec.destroy_parser)(generation.parser) };
                if result != 0 {
                    tracing::error!(code = result, "cuvidDestroyVideoParser failed");
                }
                generation.parser = ptr::null_mut();
            }
            let decoder = generation.callback.decoder;
            if decoder.is_null() {
                return;
            }
            if generation.mapped_surfaces == 0 {
                // SAFETY: no mapped surface references this decoder.
                let result = unsafe { (self.nvdec.destroy_decoder)(decoder) };
                if result != 0 {
                    tracing::error!(code = result, "cuvidDestroyDecoder failed");
                }
            } else {
                self.retired.insert(
                    generation.id,
                    RetiredDecoder {
                        decoder,
                        mapped_surfaces: generation.mapped_surfaces,
                    },
                );
            }
        }

        fn release_surface(&mut self, generation: u64, device_ptr: u64) {
            if let Some(current) = self.current.as_mut()
                && current.id == generation
            {
                let decoder = current.callback.decoder;
                unmap_surface(self.nvdec, decoder, device_ptr);
                current.mapped_surfaces = current.mapped_surfaces.saturating_sub(1);
                self.surfaces.release();
                return;
            }
            let mut destroy = None;
            if let Some(retired) = self.retired.get_mut(&generation) {
                unmap_surface(self.nvdec, retired.decoder, device_ptr);
                retired.mapped_surfaces = retired.mapped_surfaces.saturating_sub(1);
                self.surfaces.release();
                if retired.mapped_surfaces == 0 {
                    destroy = Some(retired.decoder);
                }
            } else {
                tracing::error!(generation, device_ptr, "unknown NVDEC surface release");
            }
            if let Some(decoder) = destroy {
                self.retired.remove(&generation);
                // SAFETY: the last mapped surface was just unmapped.
                let result = unsafe { (self.nvdec.destroy_decoder)(decoder) };
                if result != 0 {
                    tracing::error!(code = result, "cuvidDestroyDecoder failed");
                }
            }
        }
    }

    impl Drop for Worker {
        fn drop(&mut self) {
            self.retire_current();
            for (_, retired) in self.retired.drain() {
                if retired.mapped_surfaces == 0 {
                    // SAFETY: no mapped surfaces remain for this retired generation.
                    let _ = unsafe { (self.nvdec.destroy_decoder)(retired.decoder) };
                } else {
                    tracing::error!(
                        mapped = retired.mapped_surfaces,
                        "NVDEC worker exited with mapped surfaces"
                    );
                }
            }
            self.release_context();
        }
    }

    impl Worker {
        fn release_context(&mut self) {
            if self.context.is_null() {
                return;
            }
            let mut popped = ptr::null_mut();
            // SAFETY: this worker pushed the retained primary context and owns this thread stack.
            let result = unsafe { (self.cuda.context_pop)(&mut popped) };
            if result != 0 {
                tracing::error!(code = result, "cuCtxPopCurrent_v2 failed");
            } else if popped != self.context {
                tracing::error!("CUDA context stack returned an unexpected context");
            }
            // SAFETY: all decoder/parser resources have been released and retain succeeded.
            let result = unsafe { (self.cuda.primary_context_release)(self.device) };
            if result != 0 {
                tracing::error!(code = result, "cuDevicePrimaryCtxRelease_v2 failed");
            }
            self.context = ptr::null_mut();
        }
    }

    impl CallbackState {
        fn sequence(&mut self, format: &sdk_ffi::CUVIDEOFORMAT) -> Result<i32, BackendError> {
            let parsed = parse_format(format, self.config)?;
            if let Some(existing) = self.format {
                if existing != parsed {
                    return Err(BackendError::Unsupported(
                        "NVDEC format changed without an input discontinuity".to_owned(),
                    ));
                }
                return Ok(i32::try_from(parsed.decode_surfaces).unwrap_or(i32::MAX));
            }
            let mut caps = zeroed::<sdk_ffi::CUVIDDECODECAPS>();
            caps.eCodecType = sdk_codec(self.config.codec);
            caps.eChromaFormat = sdk_ffi::cudaVideoChromaFormat_enum_cudaVideoChromaFormat_420;
            caps.nBitDepthMinus8 = 0;
            // SAFETY: caps points to a valid zeroed SDK structure.
            check_cuda("cuvidGetDecoderCaps", unsafe {
                (self.functions.get_caps)(&mut caps)
            })?;
            validate_caps(caps, parsed)?;
            let mut create = zeroed::<sdk_ffi::CUVIDDECODECREATEINFO>();
            create.ulWidth = parsed.coded_width.into();
            create.ulHeight = parsed.coded_height.into();
            create.ulNumDecodeSurfaces = parsed.decode_surfaces.into();
            create.CodecType = sdk_codec(self.config.codec);
            create.ChromaFormat = sdk_ffi::cudaVideoChromaFormat_enum_cudaVideoChromaFormat_420;
            create.ulCreationFlags =
                sdk_ffi::cudaVideoCreateFlags_enum_cudaVideoCreate_PreferCUVID.into();
            create.ulMaxWidth = self.config.max_coded_width.into();
            create.ulMaxHeight = self.config.max_coded_height.into();
            create.display_area.left = i16::try_from(format.display_area.left)
                .map_err(|_| BackendError::Unsupported("display left exceeds i16".to_owned()))?;
            create.display_area.top = i16::try_from(format.display_area.top)
                .map_err(|_| BackendError::Unsupported("display top exceeds i16".to_owned()))?;
            create.display_area.right = i16::try_from(format.display_area.right)
                .map_err(|_| BackendError::Unsupported("display right exceeds i16".to_owned()))?;
            create.display_area.bottom = i16::try_from(format.display_area.bottom)
                .map_err(|_| BackendError::Unsupported("display bottom exceeds i16".to_owned()))?;
            create.OutputFormat = sdk_ffi::cudaVideoSurfaceFormat_enum_cudaVideoSurfaceFormat_NV12;
            create.DeinterlaceMode =
                sdk_ffi::cudaVideoDeinterlaceMode_enum_cudaVideoDeinterlaceMode_Weave;
            create.ulTargetWidth = parsed.display_width.into();
            create.ulTargetHeight = parsed.display_height.into();
            create.ulNumOutputSurfaces = self.config.output_surfaces.into();
            // SAFETY: create is fully initialized and decoder output points to stable state storage.
            check_cuda("cuvidCreateDecoder", unsafe {
                (self.functions.create_decoder)(&mut self.decoder, &mut create)
            })?;
            self.format = Some(parsed);
            Ok(i32::try_from(parsed.decode_surfaces).unwrap_or(i32::MAX))
        }

        fn decode_picture(
            &mut self,
            picture: *mut sdk_ffi::CUVIDPICPARAMS,
        ) -> Result<(), BackendError> {
            if self.decoder.is_null() {
                return Err(BackendError::Processing(
                    "NVDEC decode callback ran before decoder creation".to_owned(),
                ));
            }
            // SAFETY: picture is supplied by the parser for the duration of this callback.
            check_cuda("cuvidDecodePicture", unsafe {
                (self.functions.decode_picture)(self.decoder, picture)
            })
        }
    }

    unsafe extern "C" fn sequence_callback(
        user_data: *mut c_void,
        format: *mut sdk_ffi::CUVIDEOFORMAT,
    ) -> i32 {
        callback_result(user_data, |state| {
            if format.is_null() {
                return Err(BackendError::Processing(
                    "NVDEC sequence callback received a null format".to_owned(),
                ));
            }
            // SAFETY: the parser guarantees a valid format pointer for this callback.
            state.sequence(unsafe { &*format })
        })
    }

    unsafe extern "C" fn decode_callback(
        user_data: *mut c_void,
        picture: *mut sdk_ffi::CUVIDPICPARAMS,
    ) -> i32 {
        callback_result(user_data, |state| {
            if picture.is_null() {
                return Err(BackendError::Processing(
                    "NVDEC decode callback received a null picture".to_owned(),
                ));
            }
            state.decode_picture(picture).map(|()| 1)
        })
    }

    unsafe extern "C" fn display_callback(
        user_data: *mut c_void,
        display: *mut sdk_ffi::CUVIDPARSERDISPINFO,
    ) -> i32 {
        callback_result(user_data, |state| {
            // CUVID_PKT_NOTIFY_EOS deliberately emits one null display callback after draining.
            if display.is_null() {
                return Ok(1);
            }
            // SAFETY: the parser guarantees a valid display pointer for this callback.
            state.display_queue.push(unsafe { *display });
            Ok(1)
        })
    }

    fn callback_result(
        user_data: *mut c_void,
        callback: impl FnOnce(&mut CallbackState) -> Result<i32, BackendError>,
    ) -> i32 {
        if user_data.is_null() {
            return 0;
        }
        // SAFETY: pUserData points to the boxed CallbackState for the parser lifetime.
        let state = unsafe { &mut *user_data.cast::<CallbackState>() };
        match catch_unwind(AssertUnwindSafe(|| callback(state))) {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                state.failure = Some(error);
                0
            }
            Err(_) => {
                state.failure = Some(BackendError::Processing(
                    "panic inside NVDEC parser callback".to_owned(),
                ));
                0
            }
        }
    }

    fn parse_format(
        format: &sdk_ffi::CUVIDEOFORMAT,
        config: NvdecConfig,
    ) -> Result<NvdecFormat, BackendError> {
        if format.codec != sdk_codec(config.codec) {
            return Err(BackendError::Unsupported(format!(
                "NVDEC {} decoder received a different codec",
                config.codec.name()
            )));
        }
        if format.chroma_format != sdk_ffi::cudaVideoChromaFormat_enum_cudaVideoChromaFormat_420 {
            return Err(BackendError::Unsupported(
                "NVDEC alpha accepts only 4:2:0 chroma".to_owned(),
            ));
        }
        if format.bit_depth_luma_minus8 != 0 || format.bit_depth_chroma_minus8 != 0 {
            return Err(BackendError::Unsupported(
                "NVDEC alpha accepts only 8-bit input".to_owned(),
            ));
        }
        if format.progressive_sequence == 0 {
            return Err(BackendError::Unsupported(
                "NVDEC alpha accepts only progressive input".to_owned(),
            ));
        }
        let display_width = positive_extent(format.display_area.left, format.display_area.right)?;
        let display_height = positive_extent(format.display_area.top, format.display_area.bottom)?;
        let parsed = NvdecFormat {
            coded_width: format.coded_width,
            coded_height: format.coded_height,
            display_width,
            display_height,
            fps_numerator: format.frame_rate.numerator,
            fps_denominator: format.frame_rate.denominator,
            decode_surfaces: u32::from(format.min_num_decode_surfaces.max(1)),
        };
        validate_format_policy(parsed, config)?;
        Ok(parsed)
    }

    fn validate_format_policy(
        format: NvdecFormat,
        config: NvdecConfig,
    ) -> Result<(), BackendError> {
        if format.coded_width > config.max_coded_width
            || format.coded_height > config.max_coded_height
            || format.display_width > config.max_display_width
            || format.display_height > config.max_display_height
        {
            return Err(BackendError::Unsupported(format!(
                "NVDEC input {}x{} (display {}x{}) exceeds configured maximum",
                format.coded_width,
                format.coded_height,
                format.display_width,
                format.display_height
            )));
        }
        if format.fps_numerator != 0
            && format.fps_denominator != 0
            && u64::from(format.fps_numerator)
                > u64::from(config.max_fps) * u64::from(format.fps_denominator)
        {
            return Err(BackendError::Unsupported(format!(
                "NVDEC input frame rate {}/{} exceeds {} fps",
                format.fps_numerator, format.fps_denominator, config.max_fps
            )));
        }
        Ok(())
    }

    fn validate_caps(
        caps: sdk_ffi::CUVIDDECODECAPS,
        format: NvdecFormat,
    ) -> Result<(), BackendError> {
        let nv12_bit = 1_u16 << sdk_ffi::cudaVideoSurfaceFormat_enum_cudaVideoSurfaceFormat_NV12;
        let macroblocks = u64::from(format.coded_width)
            .saturating_mul(u64::from(format.coded_height))
            .div_ceil(256);
        if caps.bIsSupported == 0
            || format.coded_width < u32::from(caps.nMinWidth)
            || format.coded_height < u32::from(caps.nMinHeight)
            || format.coded_width > caps.nMaxWidth
            || format.coded_height > caps.nMaxHeight
            || macroblocks > u64::from(caps.nMaxMBCount)
            || caps.nOutputFormatMask & nv12_bit == 0
        {
            return Err(BackendError::Unsupported(format!(
                "GPU does not support the requested 8-bit 4:2:0 NV12 decode at {}x{}",
                format.coded_width, format.coded_height,
            )));
        }
        Ok(())
    }

    fn validate_config(config: NvdecConfig) -> Result<(), BackendError> {
        if config.max_coded_width == 0
            || config.max_coded_height == 0
            || config.max_display_width == 0
            || config.max_display_height == 0
            || config.max_fps == 0
            || config.output_surfaces == 0
            || config.command_capacity == 0
        {
            return Err(BackendError::Unsupported(
                "NVDEC limits and queue capacity must be non-zero".to_owned(),
            ));
        }
        Ok(())
    }

    fn has_annex_b_start_code(data: &[u8]) -> bool {
        data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
    }

    const fn sdk_codec(codec: NvdecCodec) -> sdk_ffi::cudaVideoCodec {
        match codec {
            NvdecCodec::H264 => sdk_ffi::cudaVideoCodec_enum_cudaVideoCodec_H264,
            NvdecCodec::Hevc => sdk_ffi::cudaVideoCodec_enum_cudaVideoCodec_HEVC,
        }
    }

    fn positive_extent(start: i32, end: i32) -> Result<u32, BackendError> {
        let extent = end
            .checked_sub(start)
            .filter(|extent| *extent > 0)
            .ok_or_else(|| {
                BackendError::Unsupported(format!("invalid display extent {start}..{end}"))
            })?;
        u32::try_from(extent).map_err(|_| {
            BackendError::Unsupported(format!("display extent {start}..{end} exceeds u32"))
        })
    }

    fn to_90khz(timestamp: Timestamp) -> i64 {
        let ticks =
            i128::from(timestamp.ticks) * i128::from(CLOCK_RATE) / i128::from(timestamp.timescale);
        ticks.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    }

    fn unmap_surface(functions: NvdecFunctions, decoder: sdk_ffi::CUvideodecoder, device_ptr: u64) {
        // SAFETY: release messages contain pointers returned for this decoder generation.
        let result = unsafe { (functions.unmap_frame)(decoder, device_ptr) };
        if result != 0 {
            tracing::error!(code = result, device_ptr, "cuvidUnmapVideoFrame64 failed");
        }
    }

    fn load_cuda_functions(libraries: &NvidiaLibraries) -> Result<CudaFunctions, BackendError> {
        // SAFETY: signatures are generated from the pinned SDK 13.0 ABI headers.
        unsafe {
            Ok(CudaFunctions {
                device_get: load_symbol(&libraries._cuda, b"cuDeviceGet\0", "cuDeviceGet")?,
                primary_context_retain: load_symbol(
                    &libraries._cuda,
                    b"cuDevicePrimaryCtxRetain\0",
                    "cuDevicePrimaryCtxRetain",
                )?,
                primary_context_release: load_symbol(
                    &libraries._cuda,
                    b"cuDevicePrimaryCtxRelease_v2\0",
                    "cuDevicePrimaryCtxRelease_v2",
                )?,
                context_push: load_symbol(
                    &libraries._cuda,
                    b"cuCtxPushCurrent_v2\0",
                    "cuCtxPushCurrent_v2",
                )?,
                context_pop: load_symbol(
                    &libraries._cuda,
                    b"cuCtxPopCurrent_v2\0",
                    "cuCtxPopCurrent_v2",
                )?,
            })
        }
    }

    fn load_nvdec_functions(libraries: &NvidiaLibraries) -> Result<NvdecFunctions, BackendError> {
        // SAFETY: signatures are generated from the pinned SDK 13.0 ABI headers.
        unsafe {
            Ok(NvdecFunctions {
                get_caps: load_symbol(
                    &libraries._nvcuvid,
                    b"cuvidGetDecoderCaps\0",
                    "cuvidGetDecoderCaps",
                )?,
                create_decoder: load_symbol(
                    &libraries._nvcuvid,
                    b"cuvidCreateDecoder\0",
                    "cuvidCreateDecoder",
                )?,
                destroy_decoder: load_symbol(
                    &libraries._nvcuvid,
                    b"cuvidDestroyDecoder\0",
                    "cuvidDestroyDecoder",
                )?,
                decode_picture: load_symbol(
                    &libraries._nvcuvid,
                    b"cuvidDecodePicture\0",
                    "cuvidDecodePicture",
                )?,
                map_frame: load_symbol(
                    &libraries._nvcuvid,
                    b"cuvidMapVideoFrame64\0",
                    "cuvidMapVideoFrame64",
                )?,
                unmap_frame: load_symbol(
                    &libraries._nvcuvid,
                    b"cuvidUnmapVideoFrame64\0",
                    "cuvidUnmapVideoFrame64",
                )?,
                create_parser: load_symbol(
                    &libraries._nvcuvid,
                    b"cuvidCreateVideoParser\0",
                    "cuvidCreateVideoParser",
                )?,
                parse_data: load_symbol(
                    &libraries._nvcuvid,
                    b"cuvidParseVideoData\0",
                    "cuvidParseVideoData",
                )?,
                destroy_parser: load_symbol(
                    &libraries._nvcuvid,
                    b"cuvidDestroyVideoParser\0",
                    "cuvidDestroyVideoParser",
                )?,
            })
        }
    }

    unsafe fn load_symbol<T: Copy>(
        library: &libloading::Library,
        name: &[u8],
        display_name: &'static str,
    ) -> Result<T, BackendError> {
        // SAFETY: caller supplies the ABI signature from generated bindings.
        unsafe { crate::symbol(library, name, display_name) }.map_err(nvidia_backend_error)
    }

    fn nvidia_backend_error(error: NvidiaError) -> BackendError {
        match error {
            NvidiaError::Library { .. } | NvidiaError::Symbol { .. } => {
                BackendError::Unavailable(error.to_string())
            }
            NvidiaError::UnsupportedNvencApi { .. } => BackendError::Unsupported(error.to_string()),
            NvidiaError::Operation { .. } => BackendError::Processing(error.to_string()),
        }
    }

    fn check_cuda(operation: &'static str, code: sdk_ffi::CUresult) -> Result<(), BackendError> {
        if code == 0 {
            Ok(())
        } else {
            Err(BackendError::Processing(format!(
                "NVIDIA operation {operation} failed with code {code}"
            )))
        }
    }

    fn zeroed<T>() -> T {
        // SAFETY: SDK input structures explicitly require reserved fields and pointers to be zero.
        unsafe { std::mem::zeroed() }
    }

    #[cfg(test)]
    mod tests {
        use aimedia_core::{
            Timestamp,
            backend::{CodecId, MediaPacket, PixelFormat, VideoDecoder},
        };

        use super::{
            NvdecCodec, NvdecConfig, NvdecDecoder, NvdecFormat, NvdecSurfaceLease,
            has_annex_b_start_code, validate_format_policy,
        };

        #[test]
        fn annex_b_access_units_are_required() {
            assert!(has_annex_b_start_code(&[0, 0, 1, 0x65]));
            assert!(has_annex_b_start_code(&[0, 0, 0, 1, 0x65]));
            assert!(!has_annex_b_start_code(&[0, 0, 0, 4, 0x65]));
        }

        #[test]
        fn alpha_policy_rejects_resolution_and_frame_rate_overflow() {
            let mut format = NvdecFormat {
                coded_width: 1_920,
                coded_height: 1_088,
                display_width: 1_920,
                display_height: 1_080,
                fps_numerator: 30_000,
                fps_denominator: 1_001,
                decode_surfaces: 8,
            };
            assert!(validate_format_policy(format, NvdecConfig::default()).is_ok());
            format.display_height = 1_081;
            assert!(validate_format_policy(format, NvdecConfig::default()).is_err());
            format.display_height = 1_080;
            format.fps_numerator = 60;
            format.fps_denominator = 1;
            assert!(validate_format_policy(format, NvdecConfig::default()).is_err());
        }

        #[tokio::test(flavor = "current_thread")]
        #[ignore = "requires an NVIDIA GPU and AIMEDIA_NVDEC_H264_FIXTURE"]
        async fn gpu_decodes_h264_fixture_and_releases_surface() {
            let fixture = std::env::var("AIMEDIA_NVDEC_H264_FIXTURE")
                .expect("AIMEDIA_NVDEC_H264_FIXTURE must point to one Annex-B IDR access unit");
            let data = std::fs::read(fixture).expect("read H.264 fixture");
            let mut decoder = NvdecDecoder::new(NvdecConfig::default()).expect("start NVDEC");
            let packet = MediaPacket {
                stream_id: 0,
                codec: CodecId::H264,
                pts: Timestamp::new(0, 90_000),
                dts: Some(Timestamp::new(0, 90_000)),
                duration: Some(Timestamp::new(3_000, 90_000)),
                keyframe: true,
                discontinuity: false,
                data: data.into(),
            };
            let mut frames = decoder
                .decode(packet.clone())
                .await
                .expect("decode fixture");
            frames.extend(decoder.flush().await.expect("flush decoder"));
            assert_eq!(frames.len(), 1);
            let frame = &frames[0];
            assert_eq!(frame.format, PixelFormat::Nv12);
            let lease = frame
                .surface
                .downcast_ref::<NvdecSurfaceLease>()
                .expect("typed NVDEC surface lease");
            assert_ne!(lease.device_ptr(), 0);
            assert!(lease.pitch() >= frame.width);

            let mut discontinuity = packet.clone();
            discontinuity.keyframe = false;
            discontinuity.discontinuity = true;
            assert!(decoder.decode(discontinuity).await.unwrap().is_empty());
            let mut resumed = packet;
            resumed.pts = Timestamp::new(3_000, 90_000);
            resumed.dts = Some(resumed.pts);
            let mut resumed_frames = decoder.decode(resumed).await.expect("resume on IDR");
            resumed_frames.extend(decoder.flush().await.expect("flush resumed decoder"));
            assert_eq!(resumed_frames.len(), 1);

            drop(frames);
            drop(resumed_frames);
            decoder.shutdown().await.expect("shutdown NVDEC");
        }

        #[tokio::test(flavor = "current_thread")]
        #[ignore = "requires an NVIDIA GPU and AIMEDIA_NVDEC_HEVC_FIXTURE"]
        async fn gpu_decodes_hevc_fixture_to_the_shared_nv12_surface_contract() {
            let fixture = std::env::var("AIMEDIA_NVDEC_HEVC_FIXTURE")
                .expect("AIMEDIA_NVDEC_HEVC_FIXTURE must point to one Annex-B IRAP access unit");
            let data = std::fs::read(fixture).expect("read HEVC fixture");
            let mut decoder = NvdecDecoder::new(NvdecConfig {
                codec: NvdecCodec::Hevc,
                ..NvdecConfig::default()
            })
            .expect("start HEVC NVDEC");
            let packet = MediaPacket {
                stream_id: 0,
                codec: CodecId::H265,
                pts: Timestamp::new(0, 90_000),
                dts: Some(Timestamp::new(0, 90_000)),
                duration: Some(Timestamp::new(3_000, 90_000)),
                keyframe: true,
                discontinuity: false,
                data: data.into(),
            };
            let mut frames = decoder.decode(packet).await.expect("decode HEVC fixture");
            frames.extend(decoder.flush().await.expect("flush HEVC decoder"));
            assert_eq!(frames.len(), 1);
            let frame = &frames[0];
            assert_eq!(frame.format, PixelFormat::Nv12);
            let lease = frame
                .surface
                .downcast_ref::<NvdecSurfaceLease>()
                .expect("typed NVDEC surface lease");
            assert_ne!(lease.device_ptr(), 0);
            assert!(lease.pitch() >= frame.width);

            drop(frames);
            decoder.shutdown().await.expect("shutdown HEVC NVDEC");
        }
    }
}

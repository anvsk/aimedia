#[cfg(not(feature = "video-codec-sdk"))]
use aimedia_core::backend::{BackendError, VideoEncoder};
#[cfg(not(feature = "video-codec-sdk"))]
use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvencConfig {
    pub device: u32,
    pub stream_id: u32,
    pub width: u32,
    pub height: u32,
    pub fps_numerator: u32,
    pub fps_denominator: u32,
    pub bitrate: u32,
    pub gop_frames: u32,
    pub surface_count: usize,
    pub command_capacity: usize,
}

impl Default for NvencConfig {
    fn default() -> Self {
        Self {
            device: 0,
            stream_id: 0,
            width: 1_920,
            height: 1_080,
            fps_numerator: 30,
            fps_denominator: 1,
            bitrate: 6_000_000,
            gop_frames: 30,
            surface_count: 4,
            command_capacity: 16,
        }
    }
}

#[cfg(feature = "video-codec-sdk")]
pub use native::NvencEncoder;

#[cfg(not(feature = "video-codec-sdk"))]
#[derive(Debug)]
pub struct NvencEncoder;

#[cfg(not(feature = "video-codec-sdk"))]
impl NvencEncoder {
    pub fn new(_config: NvencConfig) -> Result<Self, BackendError> {
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
impl VideoEncoder for NvencEncoder {
    async fn encode(
        &mut self,
        _frame: aimedia_core::backend::VideoFrame,
        _force_idr: bool,
    ) -> Result<Vec<aimedia_core::backend::MediaPacket>, BackendError> {
        Err(BackendError::Unavailable(
            "aimedia-nvidia was built without video-codec-sdk".to_owned(),
        ))
    }

    async fn flush(&mut self) -> Result<Vec<aimedia_core::backend::MediaPacket>, BackendError> {
        Ok(Vec::new())
    }
}

#[cfg(feature = "video-codec-sdk")]
mod native {
    use std::{
        ffi::c_void,
        ptr,
        sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
        thread,
        time::Duration,
    };

    use aimedia_core::{
        Timestamp,
        backend::{
            BackendError, CodecId, MediaPacket, MemoryDomain, PixelFormat, VideoEncoder, VideoFrame,
        },
    };
    use async_trait::async_trait;
    use bytes::Bytes;
    use tokio::sync::oneshot;

    use super::NvencConfig;
    use crate::{NvdecSurfaceLease, NvidiaError, NvidiaLibraries, sdk_ffi};

    const CLOCK_RATE: u32 = 90_000;

    const fn sdk_version(version: u32) -> u32 {
        sdk_ffi::NVENCAPI_VERSION | (version << 16) | (0x7 << 28)
    }

    const FUNCTION_LIST_VERSION: u32 = sdk_version(2);
    const OPEN_SESSION_VERSION: u32 = sdk_version(1);
    const PRESET_CONFIG_VERSION: u32 = sdk_version(5) | (1 << 31);
    const CONFIG_VERSION: u32 = sdk_version(9) | (1 << 31);
    const INITIALIZE_VERSION: u32 = sdk_version(7) | (1 << 31);
    const REGISTER_RESOURCE_VERSION: u32 = sdk_version(5);
    const MAP_RESOURCE_VERSION: u32 = sdk_version(4);
    const CREATE_BITSTREAM_VERSION: u32 = sdk_version(1);
    const PIC_PARAMS_VERSION: u32 = sdk_version(7) | (1 << 31);
    const LOCK_BITSTREAM_VERSION: u32 = sdk_version(2) | (1 << 31);

    const H264_GUID: sdk_ffi::GUID = sdk_ffi::GUID {
        Data1: 0x6bc8_2762,
        Data2: 0x4e63,
        Data3: 0x4ca4,
        Data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
    };
    const H264_MAIN_GUID: sdk_ffi::GUID = sdk_ffi::GUID {
        Data1: 0x60b5_c1d4,
        Data2: 0x67fe,
        Data3: 0x4790,
        Data4: [0x94, 0xd5, 0xc4, 0x72, 0x6d, 0x7b, 0x6e, 0x6d],
    };
    const P4_GUID: sdk_ffi::GUID = sdk_ffi::GUID {
        Data1: 0x90a7_b826,
        Data2: 0xdf06,
        Data3: 0x4862,
        Data4: [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
    };

    type CuDeviceGet = unsafe extern "C" fn(*mut sdk_ffi::CUdevice, i32) -> sdk_ffi::CUresult;
    type CuDevicePrimaryCtxRetain =
        unsafe extern "C" fn(*mut sdk_ffi::CUcontext, sdk_ffi::CUdevice) -> sdk_ffi::CUresult;
    type CuDevicePrimaryCtxRelease = unsafe extern "C" fn(sdk_ffi::CUdevice) -> sdk_ffi::CUresult;
    type CuCtxPushCurrent = unsafe extern "C" fn(sdk_ffi::CUcontext) -> sdk_ffi::CUresult;
    type CuCtxPopCurrent = unsafe extern "C" fn(*mut sdk_ffi::CUcontext) -> sdk_ffi::CUresult;
    type CuMemAllocPitch = unsafe extern "C" fn(
        *mut sdk_ffi::CUdeviceptr,
        *mut usize,
        usize,
        usize,
        u32,
    ) -> sdk_ffi::CUresult;
    type CuMemFree = unsafe extern "C" fn(sdk_ffi::CUdeviceptr) -> sdk_ffi::CUresult;
    type CuMemcpy2D = unsafe extern "C" fn(*const sdk_ffi::CUDA_MEMCPY2D) -> sdk_ffi::CUresult;
    type NvEncodeApiCreateInstance =
        unsafe extern "C" fn(*mut sdk_ffi::NV_ENCODE_API_FUNCTION_LIST) -> sdk_ffi::NVENCSTATUS;

    #[derive(Clone, Copy)]
    struct CudaFunctions {
        device_get: CuDeviceGet,
        primary_context_retain: CuDevicePrimaryCtxRetain,
        primary_context_release: CuDevicePrimaryCtxRelease,
        context_push: CuCtxPushCurrent,
        context_pop: CuCtxPopCurrent,
        mem_alloc_pitch: CuMemAllocPitch,
        mem_free: CuMemFree,
        memcpy_2d: CuMemcpy2D,
    }

    #[derive(Clone, Copy)]
    struct NvencFunctions {
        open_session: unsafe extern "C" fn(
            *mut sdk_ffi::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
            *mut *mut c_void,
        ) -> sdk_ffi::NVENCSTATUS,
        get_preset: unsafe extern "C" fn(
            *mut c_void,
            sdk_ffi::GUID,
            sdk_ffi::GUID,
            sdk_ffi::NV_ENC_TUNING_INFO,
            *mut sdk_ffi::NV_ENC_PRESET_CONFIG,
        ) -> sdk_ffi::NVENCSTATUS,
        initialize: unsafe extern "C" fn(
            *mut c_void,
            *mut sdk_ffi::NV_ENC_INITIALIZE_PARAMS,
        ) -> sdk_ffi::NVENCSTATUS,
        register: unsafe extern "C" fn(
            *mut c_void,
            *mut sdk_ffi::NV_ENC_REGISTER_RESOURCE,
        ) -> sdk_ffi::NVENCSTATUS,
        unregister: unsafe extern "C" fn(
            *mut c_void,
            sdk_ffi::NV_ENC_REGISTERED_PTR,
        ) -> sdk_ffi::NVENCSTATUS,
        map: unsafe extern "C" fn(
            *mut c_void,
            *mut sdk_ffi::NV_ENC_MAP_INPUT_RESOURCE,
        ) -> sdk_ffi::NVENCSTATUS,
        unmap: unsafe extern "C" fn(*mut c_void, sdk_ffi::NV_ENC_INPUT_PTR) -> sdk_ffi::NVENCSTATUS,
        create_bitstream: unsafe extern "C" fn(
            *mut c_void,
            *mut sdk_ffi::NV_ENC_CREATE_BITSTREAM_BUFFER,
        ) -> sdk_ffi::NVENCSTATUS,
        destroy_bitstream:
            unsafe extern "C" fn(*mut c_void, sdk_ffi::NV_ENC_OUTPUT_PTR) -> sdk_ffi::NVENCSTATUS,
        encode: unsafe extern "C" fn(
            *mut c_void,
            *mut sdk_ffi::NV_ENC_PIC_PARAMS,
        ) -> sdk_ffi::NVENCSTATUS,
        lock: unsafe extern "C" fn(
            *mut c_void,
            *mut sdk_ffi::NV_ENC_LOCK_BITSTREAM,
        ) -> sdk_ffi::NVENCSTATUS,
        unlock:
            unsafe extern "C" fn(*mut c_void, sdk_ffi::NV_ENC_OUTPUT_PTR) -> sdk_ffi::NVENCSTATUS,
        last_error: unsafe extern "C" fn(*mut c_void) -> *const std::ffi::c_char,
        destroy: unsafe extern "C" fn(*mut c_void) -> sdk_ffi::NVENCSTATUS,
    }

    pub struct NvencEncoder {
        commands: SyncSender<Command>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl std::fmt::Debug for NvencEncoder {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("NvencEncoder")
                .finish_non_exhaustive()
        }
    }

    enum Command {
        Encode {
            frame: VideoFrame,
            force_idr: bool,
            response: oneshot::Sender<Result<Vec<MediaPacket>, BackendError>>,
        },
        Flush {
            response: oneshot::Sender<Result<Vec<MediaPacket>, BackendError>>,
        },
        Shutdown,
    }

    impl NvencEncoder {
        pub fn new(config: NvencConfig) -> Result<Self, BackendError> {
            validate_config(config)?;
            let (commands, receiver) = mpsc::sync_channel(config.command_capacity);
            let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
            let worker = thread::Builder::new()
                .name(format!("aimedia-nvenc-{}", config.device))
                .spawn(move || match Worker::new(config) {
                    Ok(worker) => {
                        let _ = ready_sender.send(Ok(()));
                        worker.run(receiver);
                    }
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                    }
                })
                .map_err(|error| {
                    BackendError::Unavailable(format!("could not start NVENC worker: {error}"))
                })?;
            ready_receiver.recv().map_err(|error| {
                BackendError::Unavailable(format!("NVENC worker exited during startup: {error}"))
            })??;
            Ok(Self {
                commands,
                worker: Some(worker),
            })
        }

        pub async fn shutdown(mut self) -> Result<(), BackendError> {
            self.commands.send(Command::Shutdown).map_err(|_| {
                BackendError::Processing("NVENC worker command queue closed".to_owned())
            })?;
            let Some(worker) = self.worker.take() else {
                return Ok(());
            };
            tokio::task::spawn_blocking(move || worker.join())
                .await
                .map_err(|error| {
                    BackendError::Processing(format!("NVENC shutdown task failed: {error}"))
                })?
                .map_err(|_| BackendError::Processing("NVENC worker panicked".to_owned()))
        }

        async fn request(
            &self,
            command: impl FnOnce(oneshot::Sender<Result<Vec<MediaPacket>, BackendError>>) -> Command,
        ) -> Result<Vec<MediaPacket>, BackendError> {
            let (sender, receiver) = oneshot::channel();
            self.commands.send(command(sender)).map_err(|_| {
                BackendError::Processing("NVENC worker command queue closed".to_owned())
            })?;
            receiver.await.map_err(|_| {
                BackendError::Processing("NVENC worker response channel closed".to_owned())
            })?
        }
    }

    #[async_trait]
    impl VideoEncoder for NvencEncoder {
        async fn encode(
            &mut self,
            frame: VideoFrame,
            force_idr: bool,
        ) -> Result<Vec<MediaPacket>, BackendError> {
            self.request(|response| Command::Encode {
                frame,
                force_idr,
                response,
            })
            .await
        }

        async fn flush(&mut self) -> Result<Vec<MediaPacket>, BackendError> {
            self.request(|response| Command::Flush { response }).await
        }
    }

    impl Drop for NvencEncoder {
        fn drop(&mut self) {
            let _ = self.commands.try_send(Command::Shutdown);
        }
    }

    struct SurfaceSlot {
        device_ptr: sdk_ffi::CUdeviceptr,
        pitch: usize,
        registered: sdk_ffi::NV_ENC_REGISTERED_PTR,
        mapped: sdk_ffi::NV_ENC_INPUT_PTR,
        bitstream: sdk_ffi::NV_ENC_OUTPUT_PTR,
    }

    struct Worker {
        _libraries: std::sync::Arc<NvidiaLibraries>,
        cuda: CudaFunctions,
        nvenc: NvencFunctions,
        device: sdk_ffi::CUdevice,
        context: sdk_ffi::CUcontext,
        encoder: *mut c_void,
        config: NvencConfig,
        surfaces: Vec<SurfaceSlot>,
        next_surface: usize,
        frame_index: u32,
        flushed: bool,
    }

    impl Worker {
        fn new(config: NvencConfig) -> Result<Self, BackendError> {
            let libraries = NvidiaLibraries::load().map_err(nvidia_backend_error)?;
            let cuda = load_cuda_functions(&libraries)?;
            let nvenc = load_nvenc_functions(&libraries)?;
            let mut device = 0;
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
            check_cuda("cuDevicePrimaryCtxRetain", unsafe {
                (cuda.primary_context_retain)(&mut context, device)
            })?;
            if let Err(error) = check_cuda("cuCtxPushCurrent_v2", unsafe {
                (cuda.context_push)(context)
            }) {
                let _ = unsafe { (cuda.primary_context_release)(device) };
                return Err(error);
            }
            let mut worker = Self {
                _libraries: libraries,
                cuda,
                nvenc,
                device,
                context,
                encoder: ptr::null_mut(),
                config,
                surfaces: Vec::with_capacity(config.surface_count),
                next_surface: 0,
                frame_index: 0,
                flushed: false,
            };
            worker.open_encoder()?;
            worker.create_surfaces()?;
            Ok(worker)
        }

        fn run(mut self, receiver: Receiver<Command>) {
            let mut shutting_down = false;
            loop {
                match receiver.recv_timeout(Duration::from_millis(5)) {
                    Ok(Command::Encode {
                        frame,
                        force_idr,
                        response,
                    }) => {
                        let result = if shutting_down {
                            Err(BackendError::Processing(
                                "NVENC worker is shutting down".to_owned(),
                            ))
                        } else {
                            self.encode(frame, force_idr)
                        };
                        let _ = response.send(result);
                    }
                    Ok(Command::Flush { response }) => {
                        let result = self.flush().map(|()| Vec::new());
                        let _ = response.send(result);
                    }
                    Ok(Command::Shutdown) => shutting_down = true,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => shutting_down = true,
                }
                if shutting_down {
                    let _ = self.flush();
                    break;
                }
            }
        }

        fn open_encoder(&mut self) -> Result<(), BackendError> {
            let mut open = zeroed::<sdk_ffi::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>();
            open.version = OPEN_SESSION_VERSION;
            open.deviceType = sdk_ffi::_NV_ENC_DEVICE_TYPE_NV_ENC_DEVICE_TYPE_CUDA;
            open.device = self.context.cast::<c_void>();
            open.apiVersion = sdk_ffi::NVENCAPI_VERSION;
            check_nvenc("nvEncOpenEncodeSessionEx", unsafe {
                (self.nvenc.open_session)(&mut open, &mut self.encoder)
            })?;
            if self.encoder.is_null() {
                return Err(BackendError::Processing(
                    "NVENC returned an empty encoder session".to_owned(),
                ));
            }

            let mut preset = zeroed::<sdk_ffi::NV_ENC_PRESET_CONFIG>();
            preset.version = PRESET_CONFIG_VERSION;
            preset.presetCfg.version = CONFIG_VERSION;
            check_nvenc("nvEncGetEncodePresetConfigEx", unsafe {
                (self.nvenc.get_preset)(
                    self.encoder,
                    H264_GUID,
                    P4_GUID,
                    sdk_ffi::NV_ENC_TUNING_INFO_NV_ENC_TUNING_INFO_LOW_LATENCY,
                    &mut preset,
                )
            })?;
            let encode_config = &mut preset.presetCfg;
            encode_config.profileGUID = H264_MAIN_GUID;
            encode_config.gopLength = self.config.gop_frames;
            encode_config.frameIntervalP = 1;
            encode_config.frameFieldMode =
                sdk_ffi::_NV_ENC_PARAMS_FRAME_FIELD_MODE_NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
            encode_config.rcParams.rateControlMode =
                sdk_ffi::_NV_ENC_PARAMS_RC_MODE_NV_ENC_PARAMS_RC_CBR;
            encode_config.rcParams.averageBitRate = self.config.bitrate;
            encode_config.rcParams.maxBitRate = self.config.bitrate;
            encode_config.rcParams.lookaheadDepth = 0;
            encode_config.rcParams.set_enableLookahead(0);
            encode_config.rcParams.set_enableAQ(0);
            encode_config.rcParams.set_enableTemporalAQ(0);
            encode_config.rcParams.set_zeroReorderDelay(1);
            encode_config.rcParams.multiPass =
                sdk_ffi::_NV_ENC_MULTI_PASS_NV_ENC_MULTI_PASS_DISABLED;
            let h264 = unsafe { &mut encode_config.encodeCodecConfig.h264Config };
            h264.idrPeriod = self.config.gop_frames;
            h264.set_repeatSPSPPS(1);

            let mut initialize = zeroed::<sdk_ffi::NV_ENC_INITIALIZE_PARAMS>();
            initialize.version = INITIALIZE_VERSION;
            initialize.encodeGUID = H264_GUID;
            initialize.presetGUID = P4_GUID;
            initialize.encodeWidth = self.config.width;
            initialize.encodeHeight = self.config.height;
            initialize.darWidth = self.config.width;
            initialize.darHeight = self.config.height;
            initialize.frameRateNum = self.config.fps_numerator;
            initialize.frameRateDen = self.config.fps_denominator;
            initialize.enableEncodeAsync = 0;
            initialize.enablePTD = 1;
            initialize.encodeConfig = encode_config;
            initialize.maxEncodeWidth = self.config.width;
            initialize.maxEncodeHeight = self.config.height;
            initialize.tuningInfo = sdk_ffi::NV_ENC_TUNING_INFO_NV_ENC_TUNING_INFO_LOW_LATENCY;
            initialize.bufferFormat = sdk_ffi::_NV_ENC_BUFFER_FORMAT_NV_ENC_BUFFER_FORMAT_NV12;
            let status = unsafe { (self.nvenc.initialize)(self.encoder, &mut initialize) };
            self.check("nvEncInitializeEncoder", status)
        }

        fn create_surfaces(&mut self) -> Result<(), BackendError> {
            let rows = usize::try_from(self.config.height)
                .ok()
                .and_then(|height| height.checked_add(height / 2))
                .ok_or_else(|| {
                    BackendError::Unsupported("NV12 height overflows usize".to_owned())
                })?;
            for _ in 0..self.config.surface_count {
                let mut device_ptr = 0;
                let mut pitch = 0;
                check_cuda("cuMemAllocPitch_v2", unsafe {
                    (self.cuda.mem_alloc_pitch)(
                        &mut device_ptr,
                        &mut pitch,
                        self.config.width as usize,
                        rows,
                        16,
                    )
                })?;
                if device_ptr == 0 || pitch == 0 {
                    if device_ptr != 0 {
                        let _ = unsafe { (self.cuda.mem_free)(device_ptr) };
                    }
                    return Err(BackendError::Processing(
                        "CUDA returned an empty NVENC input allocation".to_owned(),
                    ));
                }
                self.surfaces.push(SurfaceSlot {
                    device_ptr,
                    pitch,
                    registered: ptr::null_mut(),
                    mapped: ptr::null_mut(),
                    bitstream: ptr::null_mut(),
                });
                let slot = self.surfaces.last_mut().expect("surface was just pushed");
                let pitch_u32 = u32::try_from(slot.pitch).map_err(|_| {
                    BackendError::Unsupported("NVENC CUDA pitch exceeds u32".to_owned())
                })?;
                let chroma_offset = pitch_u32.checked_mul(self.config.height).ok_or_else(|| {
                    BackendError::Unsupported("NVENC chroma offset exceeds u32".to_owned())
                })?;
                let mut register = zeroed::<sdk_ffi::NV_ENC_REGISTER_RESOURCE>();
                register.version = REGISTER_RESOURCE_VERSION;
                register.resourceType =
                    sdk_ffi::_NV_ENC_INPUT_RESOURCE_TYPE_NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
                register.width = self.config.width;
                register.height = self.config.height;
                register.pitch = pitch_u32;
                register.resourceToRegister = (slot.device_ptr as usize) as *mut c_void;
                register.bufferFormat = sdk_ffi::_NV_ENC_BUFFER_FORMAT_NV_ENC_BUFFER_FORMAT_NV12;
                register.bufferUsage = sdk_ffi::_NV_ENC_BUFFER_USAGE_NV_ENC_INPUT_IMAGE;
                register.chromaOffset[0] = chroma_offset;
                register.chromaOffsetIn[0] = chroma_offset;
                check_nvenc("nvEncRegisterResource", unsafe {
                    (self.nvenc.register)(self.encoder, &mut register)
                })?;
                slot.registered = register.registeredResource;
                if slot.registered.is_null() {
                    return Err(BackendError::Processing(
                        "NVENC returned an empty registered input resource".to_owned(),
                    ));
                }

                let mut bitstream = zeroed::<sdk_ffi::NV_ENC_CREATE_BITSTREAM_BUFFER>();
                bitstream.version = CREATE_BITSTREAM_VERSION;
                check_nvenc("nvEncCreateBitstreamBuffer", unsafe {
                    (self.nvenc.create_bitstream)(self.encoder, &mut bitstream)
                })?;
                slot.bitstream = bitstream.bitstreamBuffer;
                if slot.bitstream.is_null() {
                    return Err(BackendError::Processing(
                        "NVENC returned an empty bitstream buffer".to_owned(),
                    ));
                }
            }
            Ok(())
        }

        fn encode(
            &mut self,
            frame: VideoFrame,
            force_idr: bool,
        ) -> Result<Vec<MediaPacket>, BackendError> {
            if self.flushed {
                return Err(BackendError::Processing(
                    "NVENC cannot encode after EOS".to_owned(),
                ));
            }
            validate_frame(self.config, &frame)?;
            let source = frame
                .surface
                .downcast_ref::<NvdecSurfaceLease>()
                .ok_or_else(|| {
                    BackendError::Unsupported(
                        "NVENC currently accepts only aimedia NVDEC surface leases".to_owned(),
                    )
                })?;
            if source.width() != self.config.width || source.height() != self.config.height {
                return Err(BackendError::Unsupported(format!(
                    "NVDEC lease is {}x{}, expected {}x{}",
                    source.width(),
                    source.height(),
                    self.config.width,
                    self.config.height
                )));
            }
            let input_timestamp = to_90khz(frame.pts)?;
            let input_duration = frame_duration_90khz(self.config)?;
            let slot_index = self.next_surface;
            self.next_surface = (self.next_surface + 1) % self.surfaces.len();
            let input_pitch = u32::try_from(self.surfaces[slot_index].pitch).map_err(|_| {
                BackendError::Unsupported("NVENC CUDA pitch exceeds u32".to_owned())
            })?;
            self.copy_nv12(source, slot_index)?;

            let slot = &mut self.surfaces[slot_index];
            let mut map = zeroed::<sdk_ffi::NV_ENC_MAP_INPUT_RESOURCE>();
            map.version = MAP_RESOURCE_VERSION;
            map.registeredResource = slot.registered;
            check_nvenc("nvEncMapInputResource", unsafe {
                (self.nvenc.map)(self.encoder, &mut map)
            })?;
            slot.mapped = map.mappedResource;
            if slot.mapped.is_null() {
                return Err(BackendError::Processing(
                    "NVENC returned an empty mapped input surface".to_owned(),
                ));
            }

            let first_or_forced = self.frame_index == 0 || force_idr;
            let mut picture = zeroed::<sdk_ffi::NV_ENC_PIC_PARAMS>();
            picture.version = PIC_PARAMS_VERSION;
            picture.inputWidth = self.config.width;
            picture.inputHeight = self.config.height;
            picture.inputPitch = input_pitch;
            picture.frameIdx = self.frame_index;
            picture.inputTimeStamp = input_timestamp;
            picture.inputDuration = input_duration;
            picture.inputBuffer = slot.mapped;
            picture.outputBitstream = slot.bitstream;
            picture.bufferFmt = sdk_ffi::_NV_ENC_BUFFER_FORMAT_NV_ENC_BUFFER_FORMAT_NV12;
            picture.pictureStruct = sdk_ffi::_NV_ENC_PIC_STRUCT_NV_ENC_PIC_STRUCT_FRAME;
            if first_or_forced {
                picture.encodePicFlags = sdk_ffi::_NV_ENC_PIC_FLAGS_NV_ENC_PIC_FLAG_FORCEIDR
                    | sdk_ffi::_NV_ENC_PIC_FLAGS_NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
            }
            let status = unsafe { (self.nvenc.encode)(self.encoder, &mut picture) };
            if status == sdk_ffi::_NVENCSTATUS_NV_ENC_ERR_NEED_MORE_INPUT {
                self.unmap_slot(slot_index);
                return Err(BackendError::Processing(
                    "NVENC buffered a frame despite no-B-frame/no-lookahead configuration"
                        .to_owned(),
                ));
            }
            if let Err(error) = check_nvenc("nvEncEncodePicture", status) {
                self.unmap_slot(slot_index);
                return Err(error);
            }

            let mut lock = zeroed::<sdk_ffi::NV_ENC_LOCK_BITSTREAM>();
            lock.version = LOCK_BITSTREAM_VERSION;
            lock.outputBitstream = slot.bitstream;
            lock.set_doNotWait(0);
            if let Err(error) = check_nvenc("nvEncLockBitstream", unsafe {
                (self.nvenc.lock)(self.encoder, &mut lock)
            }) {
                self.unmap_slot(slot_index);
                return Err(error);
            }
            if lock.bitstreamBufferPtr.is_null() || lock.bitstreamSizeInBytes == 0 {
                let _ = unsafe { (self.nvenc.unlock)(self.encoder, slot.bitstream) };
                self.unmap_slot(slot_index);
                return Err(BackendError::Processing(
                    "NVENC returned an empty bitstream".to_owned(),
                ));
            }
            let encoded = unsafe {
                std::slice::from_raw_parts(
                    lock.bitstreamBufferPtr.cast::<u8>(),
                    lock.bitstreamSizeInBytes as usize,
                )
            };
            let data = Bytes::copy_from_slice(encoded);
            let keyframe = matches!(
                lock.pictureType,
                sdk_ffi::_NV_ENC_PIC_TYPE_NV_ENC_PIC_TYPE_IDR
                    | sdk_ffi::_NV_ENC_PIC_TYPE_NV_ENC_PIC_TYPE_I
            );
            let output_pts = i64::try_from(lock.outputTimeStamp).map_err(|_| {
                BackendError::Processing("NVENC output timestamp exceeds i64".to_owned())
            });
            let unlock_status = unsafe { (self.nvenc.unlock)(self.encoder, slot.bitstream) };
            self.unmap_slot(slot_index);
            check_nvenc("nvEncUnlockBitstream", unlock_status)?;
            let output_pts = output_pts?;
            self.frame_index = self.frame_index.wrapping_add(1);
            let pts = Timestamp::new(output_pts, CLOCK_RATE);
            Ok(vec![MediaPacket {
                stream_id: self.config.stream_id,
                codec: CodecId::H264,
                pts,
                dts: Some(pts),
                duration: Some(Timestamp::new(
                    i64::from(self.config.fps_denominator),
                    self.config.fps_numerator,
                )),
                keyframe,
                discontinuity: false,
                data,
            }])
        }

        fn copy_nv12(
            &self,
            source: &NvdecSurfaceLease,
            slot_index: usize,
        ) -> Result<(), BackendError> {
            let slot = &self.surfaces[slot_index];
            let source_pitch = source.pitch() as usize;
            let height = self.config.height as usize;
            let source_chroma = source
                .device_ptr()
                .checked_add((source_pitch * height) as u64)
                .ok_or_else(|| {
                    BackendError::Processing("NVDEC chroma pointer overflow".to_owned())
                })?;
            let target_chroma = slot
                .device_ptr
                .checked_add((slot.pitch * height) as u64)
                .ok_or_else(|| {
                    BackendError::Processing("NVENC chroma pointer overflow".to_owned())
                })?;
            self.copy_plane(
                source.device_ptr(),
                source_pitch,
                slot.device_ptr,
                slot.pitch,
                height,
            )?;
            self.copy_plane(
                source_chroma,
                source_pitch,
                target_chroma,
                slot.pitch,
                height / 2,
            )
        }

        fn copy_plane(
            &self,
            source: sdk_ffi::CUdeviceptr,
            source_pitch: usize,
            target: sdk_ffi::CUdeviceptr,
            target_pitch: usize,
            height: usize,
        ) -> Result<(), BackendError> {
            let mut copy = zeroed::<sdk_ffi::CUDA_MEMCPY2D>();
            copy.srcMemoryType = sdk_ffi::CUmemorytype_enum_CU_MEMORYTYPE_DEVICE;
            copy.srcDevice = source;
            copy.srcPitch = source_pitch;
            copy.dstMemoryType = sdk_ffi::CUmemorytype_enum_CU_MEMORYTYPE_DEVICE;
            copy.dstDevice = target;
            copy.dstPitch = target_pitch;
            copy.WidthInBytes = self.config.width as usize;
            copy.Height = height;
            check_cuda("cuMemcpy2D_v2", unsafe { (self.cuda.memcpy_2d)(&copy) })
        }

        fn unmap_slot(&mut self, slot_index: usize) {
            let slot = &mut self.surfaces[slot_index];
            if slot.mapped.is_null() {
                return;
            }
            let status = unsafe { (self.nvenc.unmap)(self.encoder, slot.mapped) };
            if status != sdk_ffi::_NVENCSTATUS_NV_ENC_SUCCESS {
                tracing::error!(code = status, "nvEncUnmapInputResource failed");
            }
            slot.mapped = ptr::null_mut();
        }

        fn flush(&mut self) -> Result<(), BackendError> {
            if self.flushed || self.encoder.is_null() {
                return Ok(());
            }
            let mut picture = zeroed::<sdk_ffi::NV_ENC_PIC_PARAMS>();
            picture.version = PIC_PARAMS_VERSION;
            picture.encodePicFlags = sdk_ffi::_NV_ENC_PIC_FLAGS_NV_ENC_PIC_FLAG_EOS;
            check_nvenc("nvEncEncodePicture(EOS)", unsafe {
                (self.nvenc.encode)(self.encoder, &mut picture)
            })?;
            self.flushed = true;
            Ok(())
        }

        fn release_context(&mut self) {
            if self.context.is_null() {
                return;
            }
            let mut popped = ptr::null_mut();
            let result = unsafe { (self.cuda.context_pop)(&mut popped) };
            if result != 0 {
                tracing::error!(code = result, "cuCtxPopCurrent_v2 failed");
            } else if popped != self.context {
                tracing::error!("CUDA context stack returned an unexpected context");
            }
            let result = unsafe { (self.cuda.primary_context_release)(self.device) };
            if result != 0 {
                tracing::error!(code = result, "cuDevicePrimaryCtxRelease_v2 failed");
            }
            self.context = ptr::null_mut();
        }

        fn check(
            &self,
            operation: &'static str,
            code: sdk_ffi::NVENCSTATUS,
        ) -> Result<(), BackendError> {
            if code == sdk_ffi::_NVENCSTATUS_NV_ENC_SUCCESS {
                return Ok(());
            }
            let detail = unsafe {
                let pointer = (self.nvenc.last_error)(self.encoder);
                if pointer.is_null() {
                    None
                } else {
                    std::ffi::CStr::from_ptr(pointer)
                        .to_str()
                        .ok()
                        .map(str::to_owned)
                }
            };
            Err(BackendError::Processing(match detail {
                Some(detail) if !detail.is_empty() => {
                    format!("NVIDIA operation {operation} failed with code {code}: {detail}")
                }
                _ => format!("NVIDIA operation {operation} failed with code {code}"),
            }))
        }
    }

    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.flush();
            for slot in &mut self.surfaces {
                if !slot.mapped.is_null() && !self.encoder.is_null() {
                    let _ = unsafe { (self.nvenc.unmap)(self.encoder, slot.mapped) };
                    slot.mapped = ptr::null_mut();
                }
                if !slot.bitstream.is_null() && !self.encoder.is_null() {
                    let _ = unsafe { (self.nvenc.destroy_bitstream)(self.encoder, slot.bitstream) };
                    slot.bitstream = ptr::null_mut();
                }
                if !slot.registered.is_null() && !self.encoder.is_null() {
                    let _ = unsafe { (self.nvenc.unregister)(self.encoder, slot.registered) };
                    slot.registered = ptr::null_mut();
                }
                if slot.device_ptr != 0 {
                    let _ = unsafe { (self.cuda.mem_free)(slot.device_ptr) };
                    slot.device_ptr = 0;
                }
            }
            if !self.encoder.is_null() {
                let status = unsafe { (self.nvenc.destroy)(self.encoder) };
                if status != sdk_ffi::_NVENCSTATUS_NV_ENC_SUCCESS {
                    tracing::error!(code = status, "nvEncDestroyEncoder failed");
                }
                self.encoder = ptr::null_mut();
            }
            self.release_context();
        }
    }

    fn validate_config(config: NvencConfig) -> Result<(), BackendError> {
        if config.width == 0
            || config.height == 0
            || config.width > 1_920
            || config.height > 1_080
            || config.width % 2 != 0
            || config.height % 2 != 0
        {
            return Err(BackendError::Unsupported(format!(
                "NVENC alpha accepts even H.264 dimensions up to 1920x1080, got {}x{}",
                config.width, config.height
            )));
        }
        if config.fps_numerator == 0
            || config.fps_denominator == 0
            || u64::from(config.fps_numerator) > 30 * u64::from(config.fps_denominator)
        {
            return Err(BackendError::Unsupported(format!(
                "NVENC alpha accepts frame rates up to 30 fps, got {}/{}",
                config.fps_numerator, config.fps_denominator
            )));
        }
        if config.bitrate == 0 || config.gop_frames == 0 {
            return Err(BackendError::Unsupported(
                "NVENC bitrate and GOP length must be non-zero".to_owned(),
            ));
        }
        if !(1..=16).contains(&config.surface_count) || config.command_capacity == 0 {
            return Err(BackendError::Unsupported(
                "NVENC surface count must be 1..=16 and command capacity must be non-zero"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_frame(config: NvencConfig, frame: &VideoFrame) -> Result<(), BackendError> {
        if frame.width != config.width || frame.height != config.height {
            return Err(BackendError::Unsupported(format!(
                "NVENC frame is {}x{}, expected {}x{}",
                frame.width, frame.height, config.width, config.height
            )));
        }
        if frame.format != PixelFormat::Nv12 {
            return Err(BackendError::Unsupported(format!(
                "NVENC expects NV12, got {:?}",
                frame.format
            )));
        }
        if frame.memory
            != (MemoryDomain::Cuda {
                device: config.device,
            })
        {
            return Err(BackendError::Unsupported(format!(
                "NVENC expects CUDA device {}, got {:?}",
                config.device, frame.memory
            )));
        }
        Ok(())
    }

    fn frame_duration_90khz(config: NvencConfig) -> Result<u64, BackendError> {
        (u64::from(CLOCK_RATE) * u64::from(config.fps_denominator))
            .checked_div(u64::from(config.fps_numerator))
            .filter(|duration| *duration > 0)
            .ok_or_else(|| BackendError::Unsupported("NVENC frame duration is invalid".to_owned()))
    }

    fn to_90khz(timestamp: Timestamp) -> Result<u64, BackendError> {
        let ticks =
            i128::from(timestamp.ticks) * i128::from(CLOCK_RATE) / i128::from(timestamp.timescale);
        i64::try_from(ticks)
            .ok()
            .and_then(|ticks| u64::try_from(ticks).ok())
            .ok_or_else(|| {
                BackendError::Unsupported(
                    "NVENC requires a non-negative 90 kHz timestamp within i64".to_owned(),
                )
            })
    }

    fn load_cuda_functions(libraries: &NvidiaLibraries) -> Result<CudaFunctions, BackendError> {
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
                mem_alloc_pitch: load_symbol(
                    &libraries._cuda,
                    b"cuMemAllocPitch_v2\0",
                    "cuMemAllocPitch_v2",
                )?,
                mem_free: load_symbol(&libraries._cuda, b"cuMemFree_v2\0", "cuMemFree_v2")?,
                memcpy_2d: load_symbol(&libraries._cuda, b"cuMemcpy2D_v2\0", "cuMemcpy2D_v2")?,
            })
        }
    }

    fn load_nvenc_functions(libraries: &NvidiaLibraries) -> Result<NvencFunctions, BackendError> {
        let create: NvEncodeApiCreateInstance = unsafe {
            load_symbol(
                &libraries._nvenc,
                b"NvEncodeAPICreateInstance\0",
                "NvEncodeAPICreateInstance",
            )?
        };
        let mut list = zeroed::<sdk_ffi::NV_ENCODE_API_FUNCTION_LIST>();
        list.version = FUNCTION_LIST_VERSION;
        check_nvenc("NvEncodeAPICreateInstance", unsafe { create(&mut list) })?;
        Ok(NvencFunctions {
            open_session: required(list.nvEncOpenEncodeSessionEx, "nvEncOpenEncodeSessionEx")?,
            get_preset: required(
                list.nvEncGetEncodePresetConfigEx,
                "nvEncGetEncodePresetConfigEx",
            )?,
            initialize: required(list.nvEncInitializeEncoder, "nvEncInitializeEncoder")?,
            register: required(list.nvEncRegisterResource, "nvEncRegisterResource")?,
            unregister: required(list.nvEncUnregisterResource, "nvEncUnregisterResource")?,
            map: required(list.nvEncMapInputResource, "nvEncMapInputResource")?,
            unmap: required(list.nvEncUnmapInputResource, "nvEncUnmapInputResource")?,
            create_bitstream: required(
                list.nvEncCreateBitstreamBuffer,
                "nvEncCreateBitstreamBuffer",
            )?,
            destroy_bitstream: required(
                list.nvEncDestroyBitstreamBuffer,
                "nvEncDestroyBitstreamBuffer",
            )?,
            encode: required(list.nvEncEncodePicture, "nvEncEncodePicture")?,
            lock: required(list.nvEncLockBitstream, "nvEncLockBitstream")?,
            unlock: required(list.nvEncUnlockBitstream, "nvEncUnlockBitstream")?,
            last_error: required(list.nvEncGetLastErrorString, "nvEncGetLastErrorString")?,
            destroy: required(list.nvEncDestroyEncoder, "nvEncDestroyEncoder")?,
        })
    }

    fn required<T: Copy>(value: Option<T>, name: &'static str) -> Result<T, BackendError> {
        value.ok_or_else(|| {
            BackendError::Unavailable(format!("NVENC function table is missing {name}"))
        })
    }

    unsafe fn load_symbol<T: Copy>(
        library: &libloading::Library,
        name: &[u8],
        display_name: &'static str,
    ) -> Result<T, BackendError> {
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

    fn check_nvenc(
        operation: &'static str,
        code: sdk_ffi::NVENCSTATUS,
    ) -> Result<(), BackendError> {
        if code == sdk_ffi::_NVENCSTATUS_NV_ENC_SUCCESS {
            Ok(())
        } else {
            Err(BackendError::Processing(format!(
                "NVIDIA operation {operation} failed with code {code}"
            )))
        }
    }

    fn zeroed<T>() -> T {
        unsafe { std::mem::zeroed() }
    }

    #[cfg(test)]
    mod tests {
        use aimedia_core::{
            Timestamp,
            backend::{CodecId, MediaPacket, VideoDecoder, VideoEncoder},
        };

        use crate::{NvdecConfig, NvdecDecoder};

        use super::{NvencConfig, NvencEncoder, validate_config};

        #[test]
        fn alpha_policy_rejects_odd_or_high_frame_rate_video() {
            let mut config = NvencConfig::default();
            assert!(validate_config(config).is_ok());
            config.width = 1_919;
            assert!(validate_config(config).is_err());
            config.width = 1_920;
            config.fps_numerator = 60;
            assert!(validate_config(config).is_err());
        }

        #[tokio::test(flavor = "current_thread")]
        #[ignore = "requires an NVIDIA GPU and AIMEDIA_NVDEC_H264_FIXTURE"]
        async fn gpu_decodes_then_encodes_h264_without_cpu_frame_copy() {
            let fixture = std::env::var("AIMEDIA_NVDEC_H264_FIXTURE")
                .expect("AIMEDIA_NVDEC_H264_FIXTURE must point to one Annex-B IDR access unit");
            let data = std::fs::read(fixture).expect("read H.264 fixture");
            let dimensions = 256;
            let decode_config = NvdecConfig {
                max_coded_width: dimensions,
                max_coded_height: dimensions,
                max_display_width: dimensions,
                max_display_height: dimensions,
                ..NvdecConfig::default()
            };
            let mut decoder = NvdecDecoder::new(decode_config).expect("start NVDEC");
            let input = MediaPacket {
                stream_id: 0,
                codec: CodecId::H264,
                pts: Timestamp::new(0, 90_000),
                dts: Some(Timestamp::new(0, 90_000)),
                duration: Some(Timestamp::new(3_000, 90_000)),
                keyframe: true,
                discontinuity: false,
                data: data.into(),
            };
            let mut frames = decoder.decode(input).await.expect("decode fixture");
            frames.extend(decoder.flush().await.expect("flush decoder"));
            assert_eq!(frames.len(), 1);

            let encode_config = NvencConfig {
                width: dimensions,
                height: dimensions,
                gop_frames: 30,
                ..NvencConfig::default()
            };
            let mut encoder = NvencEncoder::new(encode_config).expect("start NVENC");
            let frame = frames.pop().expect("decoded frame");
            let mut forced_frame = frame.clone();
            forced_frame.pts = Timestamp::new(3_000, 90_000);
            let packets = encoder.encode(frame, false).await.expect("encode frame");
            assert_eq!(packets.len(), 1);
            assert!(packets[0].keyframe);
            assert_eq!(packets[0].codec, CodecId::H264);
            assert!(
                packets[0]
                    .data
                    .windows(4)
                    .any(|window| window == [0, 0, 0, 1])
            );
            let forced_packets = encoder
                .encode(forced_frame, true)
                .await
                .expect("force second IDR");
            assert_eq!(forced_packets.len(), 1);
            assert!(forced_packets[0].keyframe);
            assert!(
                forced_packets[0]
                    .data
                    .windows(4)
                    .any(|window| window == [0, 0, 0, 1])
            );
            let mut verifier = NvdecDecoder::new(decode_config).expect("start verifier NVDEC");
            let mut verified = verifier
                .decode(forced_packets[0].clone())
                .await
                .expect("decode NVENC output");
            verified.extend(verifier.flush().await.expect("flush verifier NVDEC"));
            assert_eq!(verified.len(), 1);
            assert_eq!(verified[0].width, dimensions);
            assert_eq!(verified[0].height, dimensions);
            drop(verified);
            encoder.flush().await.expect("flush encoder");
            encoder.shutdown().await.expect("shutdown NVENC");
            verifier.shutdown().await.expect("shutdown verifier NVDEC");
            decoder.shutdown().await.expect("shutdown NVDEC");
        }
    }
}

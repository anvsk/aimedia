#[cfg(feature = "video-codec-sdk")]
use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[cfg(feature = "video-codec-sdk")]
use sha2::{Digest, Sha256};

#[cfg(feature = "video-codec-sdk")]
const OFFICIAL_HEADERS: [&str; 3] = ["nvEncodeAPI.h", "nvcuvid.h", "cuviddec.h"];

#[cfg(feature = "video-codec-sdk")]
const FFNV_HEADERS: [&str; 4] = [
    "nvEncodeAPI.h",
    "dynlink_nvcuvid.h",
    "dynlink_cuviddec.h",
    "dynlink_cuda.h",
];

#[cfg(feature = "video-codec-sdk")]
struct HeaderSet {
    provider: &'static str,
    include: PathBuf,
    headers: Vec<PathBuf>,
    clang_args: Vec<String>,
}

fn main() {
    println!("cargo:rerun-if-env-changed=AIMEDIA_VIDEO_CODEC_SDK_PATH");
    println!("cargo:rerun-if-env-changed=AIMEDIA_CUDA_INCLUDE_PATH");
    println!("cargo:rerun-if-env-changed=AIMEDIA_NVCODEC_HEADERS_PATH");
    println!("cargo:rerun-if-env-changed=AIMEDIA_VIDEO_CODEC_SDK_EXPECTED_SHA256");
    println!("cargo:rerun-if-changed=wrapper.h");

    #[cfg(feature = "video-codec-sdk")]
    generate_sdk_bindings();
}

#[cfg(feature = "video-codec-sdk")]
fn generate_sdk_bindings() {
    let header_set = discover_headers();
    for path in &header_set.headers {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    validate_nvenc_version(&header_set.headers[0]);
    let fingerprint = fingerprint_headers(&header_set.headers);
    if let Ok(expected) = env::var("AIMEDIA_VIDEO_CODEC_SDK_EXPECTED_SHA256") {
        let expected = expected.trim().to_ascii_lowercase();
        assert!(
            expected.is_empty() || expected == fingerprint,
            "Video Codec SDK header fingerprint mismatch: expected {expected}, got {fingerprint}"
        );
    }

    println!("cargo:warning=Video Codec SDK 13.0 header fingerprint: {fingerprint}");
    println!("cargo:rustc-env=AIMEDIA_VIDEO_CODEC_SDK_VERSION=13.0");
    println!(
        "cargo:rustc-env=AIMEDIA_VIDEO_CODEC_HEADER_PROVIDER={}",
        header_set.provider
    );
    println!("cargo:rustc-env=AIMEDIA_VIDEO_CODEC_SDK_FINGERPRINT={fingerprint}");

    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", header_set.include.display()))
        .allowlist_function("(cuvid|NvEncodeAPI|NvEnc).*")
        .allowlist_type(
            "(CUVID|CUvideo|cudaVideo|NV_ENC|GUID|CUDA_MEMCPY2D|CUmemorytype|CUcontext|CUstream|CUdeviceptr|CUdevice|CUresult|tcuvid|tcu).*")
        .allowlist_var(
            "(CUDA_VIDEO|NV_ENC|NVENC|NVENCAPI|CUVID|CUVID_PKT|CU_CTX|CU_MEMORYTYPE|CUvideopacketflags|cudaVideo).*",
        )
        .derive_debug(false)
        .derive_default(false)
        .layout_tests(false)
        .generate_comments(false);
    for argument in header_set.clang_args {
        builder = builder.clang_arg(argument);
    }
    let bindings = builder
        .generate()
        .expect("failed to generate bindings from Video Codec SDK 13.0 headers");

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo provides OUT_DIR"))
        .join("video_codec_sdk.rs");
    bindings
        .write_to_file(&output)
        .expect("failed to write generated Video Codec SDK bindings");
}

#[cfg(feature = "video-codec-sdk")]
fn discover_headers() -> HeaderSet {
    if let Some(root) = env::var_os("AIMEDIA_NVCODEC_HEADERS_PATH").map(PathBuf::from) {
        assert!(
            root.is_dir(),
            "AIMEDIA_NVCODEC_HEADERS_PATH directory {root:?} does not exist"
        );
        let include = if root.join("include/ffnvcodec").is_dir() {
            root.join("include")
        } else {
            root.clone()
        };
        let interface = include.join("ffnvcodec");
        let headers = required_headers(&interface, &FFNV_HEADERS, "nv-codec-headers n13.0.19.0");
        let readme = root.join("README");
        assert!(
            readme.is_file(),
            "AIMEDIA_NVCODEC_HEADERS_PATH must include the nv-codec-headers README"
        );
        let source = fs::read_to_string(&readme)
            .unwrap_or_else(|error| panic!("failed to read {readme:?}: {error}"));
        assert!(
            source.contains("Video Codec SDK version 13.0.19"),
            "AIMEDIA_NVCODEC_HEADERS_PATH must point to nv-codec-headers n13.0.19.0"
        );
        println!("cargo:rerun-if-changed={}", readme.display());
        return HeaderSet {
            provider: "ffmpeg/nv-codec-headers@n13.0.19.0",
            include,
            headers,
            clang_args: vec!["-DAIMEDIA_FFNV_CODEC_HEADERS=1".to_owned()],
        };
    }

    let sdk_root = required_directory("AIMEDIA_VIDEO_CODEC_SDK_PATH");
    let cuda_include = required_directory("AIMEDIA_CUDA_INCLUDE_PATH");
    let interface = sdk_root.join("Interface");
    HeaderSet {
        provider: "nvidia-video-codec-sdk",
        headers: required_headers(&interface, &OFFICIAL_HEADERS, "Video Codec SDK 13.0"),
        include: interface,
        clang_args: vec![format!("-I{}", cuda_include.display())],
    }
}

#[cfg(feature = "video-codec-sdk")]
fn required_headers(root: &Path, names: &[&str], provider: &str) -> Vec<PathBuf> {
    names
        .iter()
        .map(|name| {
            let path = root.join(name);
            assert!(path.is_file(), "{provider} header {path:?} is missing");
            path
        })
        .collect()
}

#[cfg(feature = "video-codec-sdk")]
fn required_directory(name: &str) -> PathBuf {
    let path = env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            panic!(
                "{name} is required when aimedia-nvidia/video-codec-sdk is enabled; see docker/README.md"
            )
        });
    assert!(path.is_dir(), "{name} directory {path:?} does not exist");
    path
}

#[cfg(feature = "video-codec-sdk")]
fn validate_nvenc_version(header: &Path) {
    let source = fs::read_to_string(header)
        .unwrap_or_else(|error| panic!("failed to read {header:?}: {error}"));
    let major = define_value(&source, "NVENCAPI_MAJOR_VERSION");
    let minor = define_value(&source, "NVENCAPI_MINOR_VERSION");
    assert!(
        major == Some(13) && minor == Some(0),
        "Video Codec SDK 13.0 is required; nvEncodeAPI.h reports major={major:?}, minor={minor:?}"
    );
}

#[cfg(feature = "video-codec-sdk")]
fn define_value(source: &str, name: &str) -> Option<u32> {
    source.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next()? != "#define" || fields.next()? != name {
            return None;
        }
        fields.next()?.trim_matches(['(', ')']).parse::<u32>().ok()
    })
}

#[cfg(feature = "video-codec-sdk")]
fn fingerprint_headers(headers: &[PathBuf]) -> String {
    let mut hasher = Sha256::new();
    for path in headers {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("SDK header name is UTF-8");
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(
            fs::read(path).unwrap_or_else(|error| panic!("failed to read {path:?}: {error}")),
        );
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(feature = "video-codec-sdk")]
use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[cfg(feature = "video-codec-sdk")]
use sha2::{Digest, Sha256};

#[cfg(feature = "video-codec-sdk")]
const REQUIRED_HEADERS: [&str; 3] = ["nvEncodeAPI.h", "nvcuvid.h", "cuviddec.h"];

fn main() {
    println!("cargo:rerun-if-env-changed=AIMEDIA_VIDEO_CODEC_SDK_PATH");
    println!("cargo:rerun-if-env-changed=AIMEDIA_CUDA_INCLUDE_PATH");
    println!("cargo:rerun-if-env-changed=AIMEDIA_VIDEO_CODEC_SDK_EXPECTED_SHA256");
    println!("cargo:rerun-if-changed=wrapper.h");

    #[cfg(feature = "video-codec-sdk")]
    generate_sdk_bindings();
}

#[cfg(feature = "video-codec-sdk")]
fn generate_sdk_bindings() {
    let sdk_root = required_directory("AIMEDIA_VIDEO_CODEC_SDK_PATH");
    let cuda_include = required_directory("AIMEDIA_CUDA_INCLUDE_PATH");
    let interface = sdk_root.join("Interface");

    let headers = REQUIRED_HEADERS.map(|name| {
        let path = interface.join(name);
        if !path.is_file() {
            panic!(
                "Video Codec SDK 13.0 header {:?} is missing; the BuildKit context root must contain Interface/{name}",
                path
            );
        }
        println!("cargo:rerun-if-changed={}", path.display());
        path
    });

    validate_nvenc_version(&headers[0]);
    let fingerprint = fingerprint_headers(&headers);
    if let Ok(expected) = env::var("AIMEDIA_VIDEO_CODEC_SDK_EXPECTED_SHA256") {
        let expected = expected.trim().to_ascii_lowercase();
        assert!(
            expected.is_empty() || expected == fingerprint,
            "Video Codec SDK header fingerprint mismatch: expected {expected}, got {fingerprint}"
        );
    }

    println!("cargo:warning=Video Codec SDK 13.0 header fingerprint: {fingerprint}");
    println!("cargo:rustc-env=AIMEDIA_VIDEO_CODEC_SDK_VERSION=13.0");
    println!("cargo:rustc-env=AIMEDIA_VIDEO_CODEC_SDK_FINGERPRINT={fingerprint}");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", interface.display()))
        .clang_arg(format!("-I{}", cuda_include.display()))
        .allowlist_function("(cuvid|NvEncodeAPI|NvEnc).*")
        .allowlist_type("(CUVID|cudaVideo|NV_ENC|GUID|CUcontext|CUstream|CUdeviceptr).*")
        .allowlist_var("(CUDA_VIDEO|NV_ENC|NVENC|NVENCAPI).*")
        .derive_debug(false)
        .derive_default(false)
        .layout_tests(false)
        .generate_comments(false)
        .generate()
        .expect("failed to generate bindings from Video Codec SDK 13.0 headers");

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo provides OUT_DIR"))
        .join("video_codec_sdk.rs");
    bindings
        .write_to_file(&output)
        .expect("failed to write generated Video Codec SDK bindings");
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
    assert!(path.is_dir(), "{name} directory {:?} does not exist", path);
    path
}

#[cfg(feature = "video-codec-sdk")]
fn validate_nvenc_version(header: &Path) {
    let source = fs::read_to_string(header)
        .unwrap_or_else(|error| panic!("failed to read {:?}: {error}", header));
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
fn fingerprint_headers(headers: &[PathBuf; 3]) -> String {
    let mut hasher = Sha256::new();
    for path in headers {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("SDK header name is UTF-8");
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(
            fs::read(path).unwrap_or_else(|error| panic!("failed to read {:?}: {error}", path)),
        );
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

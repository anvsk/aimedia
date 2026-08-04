use std::ffi::c_char;

pub const PLUGIN_ABI_V1: u32 = 1;

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    FastAnalyzer = 1,
    DirectorAdvisor = 2,
    Transport = 3,
    Container = 4,
    Codec = 5,
    Filter = 6,
}

/// Read-only descriptor returned by a native plugin's `aimedia_plugin_v1` symbol.
///
/// Function tables are intentionally deferred until the Rust-internal contracts have survived the
/// alpha cycle. This descriptor lets loaders reject incompatible or misclassified libraries
/// without exposing Rust's unstable ABI.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PluginDescriptorV1 {
    pub abi_version: u32,
    pub kind: PluginKind,
    pub name: *const c_char,
    pub version: *const c_char,
    pub license_spdx: *const c_char,
}

pub type PluginEntryV1 = unsafe extern "C" fn() -> *const PluginDescriptorV1;

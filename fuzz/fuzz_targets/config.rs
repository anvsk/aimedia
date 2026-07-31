#![no_main]

use aimedia_core::PipelineConfig;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(yaml) = std::str::from_utf8(data) {
        let _ = PipelineConfig::from_yaml(yaml);
    }
});

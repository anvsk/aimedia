#![no_main]

use aimedia_mpegts::{TsPacket, probe_bytes};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = probe_bytes(data);
    if data.len() == 188 {
        let _ = TsPacket::parse(data);
    }
});

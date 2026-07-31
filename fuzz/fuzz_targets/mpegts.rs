#![no_main]

use aimedia_mpegts::{StreamDemuxer, TsPacket, probe_bytes};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = probe_bytes(data);
    let mut stream = StreamDemuxer::new();
    for chunk in data.chunks(37) {
        let _ = stream.push(chunk);
    }
    let _ = stream.flush();
    if data.len() == 188 {
        let _ = TsPacket::parse(data);
    }
});

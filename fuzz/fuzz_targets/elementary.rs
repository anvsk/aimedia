#![no_main]

use aimedia_mpegts::elementary::{parse_adts_stream, parse_annex_b};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = parse_annex_b(data);
    let _ = parse_adts_stream(data);
});

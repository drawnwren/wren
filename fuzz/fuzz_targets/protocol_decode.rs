#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = wren_proto::decode_frame(data, wren_proto::DEFAULT_MAX_FRAME_BYTES);
});

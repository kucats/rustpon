#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rustpon_fuzz::exercise_urlencoded(data);
});

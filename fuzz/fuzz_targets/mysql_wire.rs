#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() <= 1_048_576 {
        my_sqweel::fuzz_mysql_packet(data);
    }
});

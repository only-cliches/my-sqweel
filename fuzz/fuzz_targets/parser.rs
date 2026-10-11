#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4096 { return; }
    if let Ok(sql) = std::str::from_utf8(data) {
        let _ = my_sqweel::sql::parse(sql);
    }
});

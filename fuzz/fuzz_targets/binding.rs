#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 2048 { return; }
    let Ok(source) = std::str::from_utf8(data) else { return; };
    let engine = my_sqweel::Engine::new(my_sqweel::EngineConfig::default());
    let _ = engine.execute_sql("CREATE TABLE fz (id INT, n DOUBLE, s VARCHAR(32), flag BOOLEAN)");
    let sql = format!("SELECT {source} FROM fz");
    let _ = engine.execute_sql(&sql);
});

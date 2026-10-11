#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 2048 { return; }
    let Ok(expr) = std::str::from_utf8(data) else { return; };
    let engine = my_sqweel::Engine::new(my_sqweel::EngineConfig::default());
    let _ = engine.execute_sql("CREATE TABLE fz (i INT, n DOUBLE, s VARCHAR(32))");
    let _ = engine.execute_sql("INSERT INTO fz VALUES (NULL, NULL, NULL), (2, 2.5, 'x'), (-1, 0, '')");
    let _ = engine.execute_sql(&format!("SELECT ({expr}) FROM fz"));
});

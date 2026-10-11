#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4096 { return; }
    let Ok(sql) = std::str::from_utf8(data) else { return; };
    let engine = my_sqweel::Engine::new(my_sqweel::EngineConfig::default());
    let _ = engine.execute_sql("CREATE TABLE p (id INT, grp INT, v VARCHAR(16))");
    let _ = engine.execute_sql("INSERT INTO p VALUES (1, 1, 'a'), (2, 1, NULL), (3, 2, 'b')");
    let _ = engine.execute_sql(sql);
});

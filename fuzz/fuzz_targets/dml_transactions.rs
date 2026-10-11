#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4096 { return; }
    let engine = my_sqweel::Engine::new(my_sqweel::EngineConfig::default());
    let mut session = engine.session();
    let _ = session.execute_sql("CREATE TABLE fz (id INT PRIMARY KEY, v INT)");
    for (i, byte) in data.iter().enumerate().take(64) {
        let id = (i % 8) as i32;
        let value = i32::from(*byte);
        let sql = match byte % 6 {
            0 => "BEGIN".to_string(),
            1 => format!("INSERT INTO fz VALUES ({id}, {value}) ON DUPLICATE KEY UPDATE v={value}"),
            2 => format!("UPDATE fz SET v={value} WHERE id={id}"),
            3 => format!("DELETE FROM fz WHERE id={id}"),
            4 => "COMMIT".to_string(),
            _ => "ROLLBACK".to_string(),
        };
        let _ = session.execute_sql(&sql);
    }
});

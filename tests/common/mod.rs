use std::sync::{Mutex, MutexGuard, OnceLock};

pub fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

#[allow(dead_code)]
pub fn temp_lux_dir(name: &str) -> String {
    std::env::temp_dir()
        .join(format!(
            "my-sqweel-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
        .to_string_lossy()
        .into_owned()
}

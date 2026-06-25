mod common;

use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use my_sqweel::server::{self, ServerConfig};
use my_sqweel::sql::engine::Engine;

fn free_loopback_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral loopback port");
    listener.local_addr().expect("read local addr")
}

fn python_executable() -> String {
    std::env::var("MEILI_PYTHON").unwrap_or_else(|_| "python3".to_string())
}

#[test]
fn official_python_meilisearch_client_smoke() {
    let _guard = common::test_lock();

    if !Path::new("tests/python/meili_client_compat.py").exists() {
        eprintln!("skipping Python Meilisearch SDK smoke test: script missing");
        return;
    }

    let python = python_executable();
    let import_check = Command::new(&python)
        .args(["-c", "import meilisearch"])
        .output();
    match import_check {
        Ok(output) if output.status.success() => {}
        _ => {
            eprintln!(
                "skipping Python Meilisearch SDK smoke test: `{python}` cannot import meilisearch"
            );
            return;
        }
    }

    let bind_addr = free_loopback_addr();
    let mut debug_addr = free_loopback_addr();
    while debug_addr.port() == bind_addr.port() {
        debug_addr = free_loopback_addr();
    }

    let cfg = ServerConfig {
        bind_addr,
        debug_addr: Some(debug_addr),
        ..ServerConfig::default()
    };
    let engine = Arc::new(Engine::default());
    let _server = server::spawn_with_engine(cfg, engine).expect("spawn my-sqweel server");

    let output = Command::new(&python)
        .arg("tests/python/meili_client_compat.py")
        .arg(format!("http://{debug_addr}"))
        .arg("masterKey")
        .output()
        .expect("run Python Meilisearch SDK compatibility script");

    assert!(
        output.status.success(),
        "Python Meilisearch SDK compatibility script failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

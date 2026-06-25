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

#[test]
fn official_js_meilisearch_client_smoke() {
    let _guard = common::test_lock();

    if !Path::new("node_modules/meilisearch").exists() {
        eprintln!("skipping JS Meilisearch SDK smoke test: node_modules/meilisearch not installed");
        return;
    }

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping JS Meilisearch SDK smoke test: node executable not found");
        return;
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

    let output = Command::new("node")
        .arg("tests/node/meili-js-client-compat.mjs")
        .arg(format!("http://{debug_addr}"))
        .arg("masterKey")
        .output()
        .expect("run JS Meilisearch SDK compatibility script");

    assert!(
        output.status.success(),
        "JS Meilisearch SDK compatibility script failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

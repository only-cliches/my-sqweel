mod endpoint;
pub use endpoint::{SqlEndpoint, SqlEndpointConfig};
mod debug_http;
mod mysql_wire;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Result, anyhow};

use crate::sql::engine::{Engine, EngineConfig};

pub use mysql_wire::{
    AccountOperation, AccountOperationAction, AccountOperationKind, AsyncAuthenticator,
    AsyncAuthenticatorHandle, AuthenticatedUser, Authentication, AuthenticationRequest, StaticUser,
    verify_mysql_native_password,
};

pub use debug_http::{DebugHttpHandle, spawn as spawn_debug_http};
use mysql_wire::WireServer;

pub(crate) struct ServerHandle {
    _sql: SqlEndpoint,
    _debug: debug_http::DebugHttpHandle,
}

#[derive(Debug, Clone)]
pub(crate) struct ServerConfig {
    pub bind_addr: SocketAddr,
    pub data_dir: Option<String>,
    pub allow_remote: bool,
    pub debug_addr: Option<SocketAddr>,
    pub engine: EngineConfig,
    /// Authentication for MariaDB wire connections. The default allows all
    /// local connections with full scope; select `EngineAccounts`, static
    /// users, or a callback to restrict access.
    pub authentication: Authentication,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:3307"
                .parse()
                .expect("valid default bind address"),
            data_dir: None,
            allow_remote: false,
            debug_addr: None,
            engine: EngineConfig::default(),
            authentication: Authentication::default(),
        }
    }
}

impl ServerConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.bind_addr.ip().is_loopback() && !self.allow_remote {
            return Err(anyhow!(
                "refusing non-loopback bind {}. Pass --allow-remote to override",
                self.bind_addr
            ));
        }
        let debug_addr = self.effective_debug_addr();
        if !debug_addr.ip().is_loopback() && !self.allow_remote {
            return Err(anyhow!(
                "refusing non-loopback debug bind {}. Pass --allow-remote to override",
                debug_addr
            ));
        }
        Ok(())
    }

    pub fn effective_debug_addr(&self) -> SocketAddr {
        self.debug_addr.unwrap_or_else(|| {
            SocketAddr::new(
                self.bind_addr.ip(),
                self.bind_addr.port().saturating_add(100),
            )
        })
    }
}

pub(crate) fn open_engine(cfg: &ServerConfig) -> Result<Arc<Engine>> {
    Ok(Arc::new(Engine::open(
        cfg.engine.clone(),
        (cfg.data_dir.as_deref()).map_or(crate::Storage::Memory, |path| {
            crate::Storage::RocksDb(path.into())
        }),
    )?))
}

pub(crate) fn run(cfg: ServerConfig) -> Result<()> {
    cfg.validate()?;
    let engine = open_engine(&cfg)?;
    run_with_engine(cfg, engine)
}

fn run_with_engine(cfg: ServerConfig, engine: Arc<Engine>) -> Result<()> {
    let _server = spawn_with_engine(cfg, engine)?;
    loop {
        thread::park();
    }
}

pub(crate) fn spawn_with_engine(cfg: ServerConfig, engine: Arc<Engine>) -> Result<ServerHandle> {
    cfg.validate()?;
    log_runtime(&cfg);
    let mut endpoint = SqlEndpointConfig::new(cfg.bind_addr, cfg.authentication.clone());
    endpoint.allow_remote = cfg.allow_remote;
    let sql = engine.spawn_sql(endpoint)?;
    let debug = start_debug_http(&cfg, engine);
    Ok(ServerHandle {
        _sql: sql,
        _debug: debug,
    })
}

fn log_runtime(cfg: &ServerConfig) {
    tracing::info!(
        "MySqweel development transactions enabled; writers serialize and readers see committed data"
    );
    if cfg.allow_remote {
        tracing::warn!(
            address = %cfg.bind_addr,
            "remote bind override enabled via --allow-remote"
        );
    }

    if let Some(path) = &cfg.data_dir {
        tracing::info!(data_dir = %path, "embedded RocksDB incremental persistence enabled");
    } else {
        tracing::info!("running with in-memory transactional storage");
    }
}

fn start_debug_http(cfg: &ServerConfig, engine: Arc<Engine>) -> debug_http::DebugHttpHandle {
    let debug_addr = cfg.effective_debug_addr();
    let handle = debug_http::spawn(debug_addr, engine);
    tracing::info!(address = %debug_addr, "debug http endpoint listening");
    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    #[test]
    fn server_handle_stops_debug_server_before_drop() {
        let wire = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let wire_addr = wire.local_addr().unwrap();
        drop(wire);
        let debug = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let debug_addr = debug.local_addr().unwrap();
        drop(debug);

        let config = ServerConfig {
            bind_addr: wire_addr,
            debug_addr: Some(debug_addr),
            ..ServerConfig::default()
        };
        let engine = open_engine(&config).unwrap();
        let handle = spawn_with_engine(config, engine).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while TcpStream::connect(debug_addr).is_err() {
            assert!(Instant::now() < deadline, "debug server did not start");
            std::thread::sleep(Duration::from_millis(10));
        }

        drop(handle);

        let deadline = Instant::now() + Duration::from_secs(2);
        while TcpStream::connect(debug_addr).is_ok() {
            assert!(
                Instant::now() < deadline,
                "debug server remained bound after shutdown"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

mod debug_http;
mod mysql_wire;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Result, anyhow};

use crate::sql::engine::{Engine, EngineConfig};

pub use mysql_wire::WireServer;

pub struct ServerHandle {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<Result<()>>>,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            match join.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!(error = %err, "server accept thread stopped with error");
                }
                Err(err) => {
                    tracing::warn!(?err, "server accept thread panicked during shutdown");
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind_addr: SocketAddr,
    pub data_dir: Option<String>,
    pub allow_remote: bool,
    pub debug_addr: Option<SocketAddr>,
    pub engine: EngineConfig,
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

pub fn open_engine(cfg: &ServerConfig) -> Result<Arc<Engine>> {
    Ok(Arc::new(Engine::open_with_data_dir(
        cfg.engine.clone(),
        cfg.data_dir.as_deref(),
    )?))
}

pub fn run(cfg: ServerConfig) -> Result<()> {
    cfg.validate()?;
    let engine = open_engine(&cfg)?;
    run_with_engine(cfg, engine)
}

pub fn run_with_engine(cfg: ServerConfig, engine: Arc<Engine>) -> Result<()> {
    cfg.validate()?;
    log_runtime(&cfg);
    start_debug_http(&cfg, engine.clone());

    let wire = WireServer::new(engine.clone());
    wire.serve(cfg.bind_addr)?;
    Ok(())
}

pub fn spawn_with_engine(cfg: ServerConfig, engine: Arc<Engine>) -> Result<ServerHandle> {
    cfg.validate()?;
    let listener = std::net::TcpListener::bind(cfg.bind_addr)?;
    log_runtime(&cfg);
    start_debug_http(&cfg, engine.clone());
    let wire = WireServer::new(engine);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let join = thread::spawn(move || {
        wire.serve_listener_until(listener, thread_stop)
            .map_err(Into::into)
    });
    Ok(ServerHandle {
        stop,
        join: Some(join),
    })
}

fn log_runtime(cfg: &ServerConfig) {
    tracing::warn!("MySqweel is development-only and must not be used for production data");
    if cfg.allow_remote {
        tracing::warn!(
            address = %cfg.bind_addr,
            "remote bind override enabled via --allow-remote"
        );
    }

    if let Some(path) = &cfg.data_dir {
        tracing::info!(data_dir = %path, "Lux-backed persistent mode enabled");
    } else {
        tracing::info!("running with in-memory embedded Lux storage");
    }
}

fn start_debug_http(cfg: &ServerConfig, engine: Arc<Engine>) {
    let debug_addr = cfg.effective_debug_addr();
    debug_http::spawn(debug_addr, engine.clone());
    tracing::info!(address = %debug_addr, "debug http endpoint listening");
}

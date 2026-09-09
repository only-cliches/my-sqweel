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
    debug: Option<debug_http::DebugHttpHandle>,
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
        // Stop the debug/search server before the caller drops its final Engine
        // reference.  The debug server owns an Engine clone and must be joined so
        // persistent storage can flush on clean dev restarts.
        self.debug.take();
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
    let _debug = start_debug_http(&cfg, engine.clone());

    let wire = WireServer::new(engine.clone());
    wire.serve(cfg.bind_addr)?;
    Ok(())
}

pub fn spawn_with_engine(cfg: ServerConfig, engine: Arc<Engine>) -> Result<ServerHandle> {
    cfg.validate()?;
    let listener = std::net::TcpListener::bind(cfg.bind_addr)?;
    log_runtime(&cfg);
    let debug = start_debug_http(&cfg, engine.clone());
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
        debug: Some(debug),
    })
}

fn log_runtime(cfg: &ServerConfig) {
    tracing::info!("MySqweel development transactions enabled; writers serialize and readers see committed data");
    if cfg.allow_remote {
        tracing::warn!(
            address = %cfg.bind_addr,
            "remote bind override enabled via --allow-remote"
        );
    }

    if let Some(path) = &cfg.data_dir {
        tracing::info!(data_dir = %path, "atomic database image persistence enabled");
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
            assert!(Instant::now() < deadline, "debug server remained bound after shutdown");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

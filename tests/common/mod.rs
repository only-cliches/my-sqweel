#![allow(dead_code)]

use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mysql::{Opts, Pool};

pub const MYSQL_DOCKER_IMAGE: &str = "mariadb:10.11.7";
pub const MYSQL_DOCKER_PASSWORD: &str = "my-sqweel";
pub const MYSQL_DOCKER_DATABASE: &str = "test";

pub enum MysqlTarget {
    External(String),
    Docker(DockerMysql),
}

impl MysqlTarget {
    pub fn url(&self) -> &str {
        match self {
            Self::External(url) => url,
            Self::Docker(container) => &container.url,
        }
    }
}

pub struct DockerMysql {
    name: String,
    url: String,
}

impl Drop for DockerMysql {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .output();
    }
}

pub fn mysql_compare_target() -> Option<MysqlTarget> {
    if let Ok(url) = std::env::var("MARIADB_COMPARE_URL") {
        return Some(MysqlTarget::External(url));
    }
    if let Ok(url) = std::env::var("MYSQL_COMPARE_URL") {
        return Some(MysqlTarget::External(url));
    }

    match start_docker_mysql() {
        Ok(container) => Some(MysqlTarget::Docker(container)),
        Err(error) => {
            if mysql_parity_required() {
                panic!("real-MySQL parity is required: {error}");
            }
            eprintln!("skipping real-MySQL comparison: {error}");
            None
        }
    }
}

pub fn mysql_parity_required() -> bool {
    ["MARIADB_PARITY_REQUIRED", "MYSQL_PARITY_REQUIRED"]
        .iter()
        .any(|name| {
            std::env::var(name).is_ok_and(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes"
                )
            })
        })
}

fn start_docker_mysql() -> Result<DockerMysql, String> {
    if !docker_available() {
        return Err(
            "MYSQL_COMPARE_URL is not set and Docker is not available from this test process"
                .to_string(),
        );
    }
    if !docker_image_available(MYSQL_DOCKER_IMAGE) {
        return Err(format!(
            "MYSQL_COMPARE_URL is not set and Docker image {MYSQL_DOCKER_IMAGE:?} is not local; run `docker pull {MYSQL_DOCKER_IMAGE}` first"
        ));
    }

    let name = format!(
        "my-sqweel-mysql-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default()
    );
    let output = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "-e",
            &format!("MYSQL_ROOT_PASSWORD={MYSQL_DOCKER_PASSWORD}"),
            "-e",
            &format!("MYSQL_DATABASE={MYSQL_DOCKER_DATABASE}"),
            "-p",
            "127.0.0.1::3306",
            MYSQL_DOCKER_IMAGE,
        ])
        .output()
        .map_err(|error| format!("failed to start Docker MySQL: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to start Docker MySQL: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let port = docker_container_port(&name)?;
    let url =
        format!("mysql://root:{MYSQL_DOCKER_PASSWORD}@127.0.0.1:{port}/{MYSQL_DOCKER_DATABASE}");
    wait_for_mysql(&url).inspect_err(|_| {
        let _ = Command::new("docker").args(["logs", &name]).status();
        let _ = Command::new("docker").args(["rm", "-f", &name]).status();
    })?;

    Ok(DockerMysql { name, url })
}

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn docker_image_available(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn docker_container_port(name: &str) -> Result<u16, String> {
    let output = Command::new("docker")
        .args(["port", name, "3306/tcp"])
        .output()
        .map_err(|error| format!("failed to inspect Docker MySQL port: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to inspect Docker MySQL port: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let endpoint = stdout
        .lines()
        .next()
        .ok_or_else(|| "Docker did not publish MySQL port 3306".to_string())?;
    endpoint
        .rsplit_once(':')
        .and_then(|(_, port)| port.trim().parse::<u16>().ok())
        .ok_or_else(|| format!("could not parse Docker MySQL port from {endpoint:?}"))
}

fn wait_for_mysql(url: &str) -> Result<(), String> {
    let opts = Opts::from_url(url).map_err(|error| format!("invalid Docker MySQL URL: {error}"))?;
    for _ in 0..90 {
        if let Ok(pool) = Pool::new(opts.clone())
            && pool.get_conn().is_ok()
        {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err("Docker MySQL did not become ready within 90 seconds".to_string())
}

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

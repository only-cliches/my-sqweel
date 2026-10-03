use super::*;
use crate::sql::engine::AuthScope;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub struct SqlEndpointConfig {
    pub bind_addr: SocketAddr,
    pub authentication: Authentication,
    pub scopes: Vec<AuthScope>,
    pub default_database: String,
    pub allow_remote: bool,
}
impl SqlEndpointConfig {
    pub fn new(bind_addr: SocketAddr, authentication: Authentication) -> Self {
        Self {
            bind_addr,
            authentication,
            scopes: vec![AuthScope::All],
            default_database: "app".into(),
            allow_remote: false,
        }
    }
    pub fn with_scopes(mut self, scopes: impl IntoIterator<Item = AuthScope>) -> Self {
        self.scopes = scopes.into_iter().collect();
        self
    }
    pub fn with_default_database(mut self, database: impl Into<String>) -> Self {
        self.default_database = database.into();
        self
    }
}

#[derive(Default)]
pub(crate) struct Clients {
    pub stopping: Arc<AtomicBool>,
    pub streams: Mutex<std::collections::HashMap<uuid::Uuid, TcpStream>>,
    pub workers: Mutex<Vec<thread::JoinHandle<()>>>,
}
impl Clients {
    fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        for stream in self.streams.lock().unwrap().values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

/// Owns one listener and all of its connections. Drop disconnects its clients.
pub struct SqlEndpoint {
    address: SocketAddr,
    clients: Arc<Clients>,
    accept: Option<thread::JoinHandle<std::io::Result<()>>>,
}
impl SqlEndpoint {
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }
    fn stop_accepting(&mut self) -> Result<()> {
        self.clients.stop();
        let result = match self.accept.take() {
            Some(accept) => accept
                .join()
                .map_err(|_| anyhow!("SQL accept worker panicked"))
                .and_then(|result| result.map_err(Into::into)),
            None => Ok(()),
        };
        self.clients.stop();
        result
    }
    pub fn shutdown(&mut self) -> Result<()> {
        let accepted = self.stop_accepting();
        let clients = self.clients.join_workers();
        accepted.and(clients)
    }
    pub async fn shutdown_async(&mut self) -> Result<()> {
        let accepted = self.stop_accepting();
        let clients = self.clients.clone();
        let finished = tokio::task::spawn_blocking(move || clients.join_workers()).await?;
        accepted.and(finished)
    }
}
impl Clients {
    fn join_workers(&self) -> Result<()> {
        let workers = std::mem::take(&mut *self.workers.lock().unwrap());
        let mut result = Ok(());
        for worker in workers {
            if worker.join().is_err() {
                result = Err(anyhow!("SQL connection worker panicked"));
            }
        }
        result
    }
}
impl Drop for SqlEndpoint {
    fn drop(&mut self) {
        let _ = self.stop_accepting();
    }
}

impl Engine {
    pub fn spawn_sql(&self, config: SqlEndpointConfig) -> Result<SqlEndpoint> {
        anyhow::ensure!(
            config.allow_remote || config.bind_addr.ip().is_loopback(),
            "non-loopback endpoints require allow_remote"
        );
        let listener = TcpListener::bind(config.bind_addr)?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let clients = Arc::new(Clients::default());
        let wire = WireServer::scoped(Arc::new(self.clone()), config, clients.clone());
        let accept = Some(
            thread::Builder::new()
                .name("sqweel-accept".into())
                .spawn(move || wire.serve_scoped(listener))?,
        );
        Ok(SqlEndpoint {
            address,
            clients,
            accept,
        })
    }
    pub async fn spawn_sql_async(&self, config: SqlEndpointConfig) -> Result<SqlEndpoint> {
        let engine = self.clone();
        tokio::task::spawn_blocking(move || engine.spawn_sql(config)).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::engine::{AuthPrivilege::*, AuthScope};
    use mysql::{Conn, OptsBuilder, prelude::Queryable};

    fn connect(endpoint: &SqlEndpoint) -> Conn {
        Conn::new(
            OptsBuilder::new()
                .ip_or_hostname(Some("127.0.0.1"))
                .tcp_port(endpoint.local_addr().port())
                .user(Some("root"))
                .db_name(Some("app"))
                .read_timeout(Some(std::time::Duration::from_secs(2))),
        )
        .unwrap()
    }

    #[test]
    fn endpoint_ceiling_covers_data_ddl_metadata_views_and_cascades() {
        let engine = Engine::default();
        engine.execute_sql("CREATE TABLE allowed (id INT PRIMARY KEY); CREATE TABLE hidden (id INT PRIMARY KEY, parent_id INT, FOREIGN KEY (parent_id) REFERENCES allowed(id) ON DELETE CASCADE); INSERT INTO allowed VALUES (1); INSERT INTO hidden VALUES (2, 1); CREATE VIEW exposed AS SELECT * FROM hidden").unwrap();
        let endpoint = engine
            .spawn_sql(
                SqlEndpointConfig::new(
                    "127.0.0.1:0".parse().unwrap(),
                    Authentication::EngineAccounts,
                )
                .with_scopes([
                    AuthScope::table(
                        "app",
                        "allowed",
                        [Select, Insert, Update, Delete, Alter, Index],
                    ),
                    AuthScope::table("app", "scratch", [Select, Insert, Create, Drop]),
                    AuthScope::table("app", "exposed", [Select]),
                ]),
            )
            .unwrap();
        let mut conn = connect(&endpoint);
        assert_eq!(
            conn.query_first::<u64, _>("SELECT id FROM allowed")
                .unwrap(),
            Some(1)
        );
        for sql in [
            "SELECT * FROM hidden",
            "SELECT * FROM exposed",
            "SHOW COLUMNS FROM hidden",
            "DROP TABLE hidden",
            "CREATE USER intruder",
            "DELETE FROM allowed WHERE id=1",
            "INSERT INTO scratch SELECT * FROM hidden",
        ] {
            assert!(conn.query_drop(sql).is_err(), "scope bypass: {sql}");
        }
        assert_eq!(
            engine.execute_sql("SELECT * FROM allowed").unwrap()[0]
                .rows
                .len(),
            1
        );
        conn.query_drop(
            "CREATE TABLE scratch (id INT PRIMARY KEY); INSERT INTO scratch VALUES (3)",
        )
        .unwrap();
        let tables: Vec<String> = conn.query("SHOW TABLES").unwrap();
        assert!(tables.contains(&"allowed".into()) && !tables.contains(&"hidden".into()));
        assert_eq!(
            conn.query_first::<u64, _>(
                "SELECT COUNT(*) FROM information_schema.tables WHERE table_name='hidden'"
            )
            .unwrap(),
            Some(0)
        );
        conn.query_drop("ALTER TABLE allowed ADD COLUMN name TEXT")
            .unwrap();
        conn.query_drop("DROP TABLE scratch").unwrap();
    }

    #[test]
    fn shutdown_disconnects_clients_rolls_back_and_keeps_other_endpoints_alive() {
        let engine = Engine::default();
        engine
            .execute_sql("CREATE TABLE items (id INT PRIMARY KEY)")
            .unwrap();
        let config =
            || SqlEndpointConfig::new("127.0.0.1:0".parse().unwrap(), Authentication::AllowAll);
        let mut first = engine.spawn_sql(config()).unwrap();
        let second = engine.spawn_sql(config()).unwrap();
        let mut client = connect(&first);
        client
            .query_drop("BEGIN; INSERT INTO items VALUES (1)")
            .unwrap();
        let address = first.local_addr();
        first.shutdown().unwrap();
        assert!(client.query_drop("SELECT 1").is_err());
        let _rebound = TcpListener::bind(address).unwrap();
        assert_eq!(
            connect(&second)
                .query_first::<u64, _>("SELECT COUNT(*) FROM items")
                .unwrap(),
            Some(0)
        );
        engine.execute_sql("INSERT INTO items VALUES (2)").unwrap();
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use mysql::{Conn, OptsBuilder, prelude::Queryable};
    use std::{sync::mpsc, time::Duration};

    struct PendingAuth(mpsc::Sender<()>);
    impl AsyncAuthenticator for PendingAuth {
        async fn authenticate(
            &self,
            _: AuthenticationRequest,
        ) -> Result<Option<AuthenticatedUser>> {
            self.0.send(()).unwrap();
            std::future::pending().await
        }
    }
    struct PendingQuery(mpsc::Sender<()>);
    impl crate::QueryFilter for PendingQuery {
        async fn filter(
            &self,
            request: &mut crate::QueryRequest,
        ) -> Result<crate::QueryFilterAction> {
            if request.sql == "SELECT 77" {
                assert!(request.context().endpoint_id.is_some());
                self.0.send(()).unwrap();
                std::future::pending().await
            } else {
                Ok(crate::QueryFilterAction::Continue)
            }
        }
    }
    fn options(endpoint: &SqlEndpoint) -> OptsBuilder {
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(endpoint.local_addr().port())
            .user(Some("root"))
    }
    #[test]
    fn shutdown_cancels_pending_authentication_and_filters() {
        let engine = Engine::default();
        let (send, receive) = mpsc::channel();
        let mut endpoint = engine
            .spawn_sql(SqlEndpointConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                Authentication::callback(PendingAuth(send)),
            ))
            .unwrap();
        let opts = options(&endpoint);
        let client = thread::spawn(move || Conn::new(opts));
        receive.recv_timeout(Duration::from_secs(3)).unwrap();
        endpoint.shutdown().unwrap();
        assert!(client.join().unwrap().is_err());

        let mut endpoint = engine
            .spawn_sql(SqlEndpointConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                Authentication::AllowAll,
            ))
            .unwrap();
        let mut conn = Conn::new(options(&endpoint)).unwrap();
        let (send, receive) = mpsc::channel();
        engine.query_filters().push(PendingQuery(send));
        let client = thread::spawn(move || conn.query_drop("SELECT 77"));
        receive.recv_timeout(Duration::from_secs(3)).unwrap();
        endpoint.shutdown().unwrap();
        assert!(client.join().unwrap().is_err());
        engine.query_filters().clear();
        engine.execute_sql("SELECT 1").unwrap();
    }
}

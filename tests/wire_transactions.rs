use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use my_sqweel::server::{
    AccountOperation, AccountOperationAction, AccountOperationKind, AsyncAuthenticator,
    AuthenticatedUser, Authentication, AuthenticationRequest, StaticUser, WireServer,
    verify_mysql_native_password,
};
use my_sqweel::sql::engine::{AuthPrivilege, AuthScope, Engine, EngineConfig};
use mysql::prelude::Queryable;
use mysql::{Conn, OptsBuilder};

struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        Self::start_with_authentication(
            Arc::new(Engine::new(EngineConfig::mysql_strict())),
            Authentication::EngineAccounts,
        )
    }

    fn start_with_authentication(engine: Arc<Engine>, authentication: Authentication) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            WireServer::with_authentication(engine, authentication)
                .serve_listener_until(listener, worker_stop)
                .unwrap();
        });
        Self {
            addr,
            stop,
            thread: Some(thread),
        }
    }

    fn connect(&self, user: &str, password: &str, db: &str) -> mysql::Result<Conn> {
        Conn::new(
            OptsBuilder::new()
                .ip_or_hostname(Some("127.0.0.1"))
                .tcp_port(self.addr.port())
                .user(Some(user))
                .pass(Some(password))
                .db_name(Some(db))
                .read_timeout(Some(Duration::from_secs(10)))
                .write_timeout(Some(Duration::from_secs(10))),
        )
    }
}

struct DirectoryAuthenticator;

impl AsyncAuthenticator for DirectoryAuthenticator {
    async fn authenticate(
        &self,
        request: AuthenticationRequest,
    ) -> anyhow::Result<Option<AuthenticatedUser>> {
        if request.username == "directory_reader"
            && verify_mysql_native_password(
                "directory-secret",
                &request.challenge,
                &request.response,
            )
        {
            return Ok(Some(AuthenticatedUser::new(
                "directory_reader",
                [AuthScope::database("app", [AuthPrivilege::Select])],
            )));
        }
        Ok(None)
    }
}

#[derive(Clone, Default)]
struct AccountDirectory {
    operations: Arc<AtomicUsize>,
}

impl AsyncAuthenticator for AccountDirectory {
    async fn authenticate(
        &self,
        request: AuthenticationRequest,
    ) -> anyhow::Result<Option<AuthenticatedUser>> {
        if request.username == "directory_admin"
            && verify_mysql_native_password(
                "directory-secret",
                &request.challenge,
                &request.response,
            )
        {
            return Ok(Some(AuthenticatedUser::new(
                "directory_admin",
                [AuthScope::All],
            )));
        }
        Ok(None)
    }

    async fn account_operation(
        &self,
        operation: AccountOperation,
    ) -> anyhow::Result<AccountOperationAction> {
        assert_eq!(operation.kind, AccountOperationKind::AlterUser);
        assert!(operation.sql.contains("external_user"));
        self.operations.fetch_add(1, Ordering::Relaxed);
        Ok(AccountOperationAction::Handled)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[test]
fn mysql_clients_authenticate_and_transactions_span_text_and_prepared_commands() {
    let server = Server::start();
    let mut admin = server.connect("root", "", "app").unwrap();
    admin.query_drop("CREATE DATABASE shard_a").unwrap();
    admin.query_drop("CREATE DATABASE shard_b").unwrap();
    admin.query_drop("USE shard_a").unwrap();
    admin
        .query_drop("CREATE TABLE records (id BIGINT PRIMARY KEY)")
        .unwrap();
    admin
        .query_drop("CREATE USER 'tenant'@'%' IDENTIFIED BY 'secret'")
        .unwrap();
    admin
        .query_drop("GRANT SELECT,INSERT,UPDATE,DELETE ON shard_a.* TO 'tenant'@'%'")
        .unwrap();

    assert!(server.connect("tenant", "wrong", "shard_a").is_err());
    assert!(server.connect("missing", "", "shard_a").is_err());
    assert!(server.connect("tenant", "secret", "shard_b").is_err());
    let mut tenant = server.connect("tenant", "secret", "shard_a").unwrap();
    assert!(tenant.query_drop("USE shard_b").is_err());
    assert!(
        tenant
            .query_drop("CREATE TABLE forbidden (id INT)")
            .is_err()
    );
    tenant.query_drop("BEGIN").unwrap();
    tenant
        .exec_drop("INSERT INTO records (id) VALUES (?)", (1,))
        .unwrap();
    let own: Option<u64> = tenant.query_first("SELECT COUNT(*) FROM records").unwrap();
    assert_eq!(own, Some(1));
    let committed: Option<u64> = admin.query_first("SELECT COUNT(*) FROM records").unwrap();
    assert_eq!(committed, Some(0));
    tenant.exec_drop("ROLLBACK", ()).unwrap();
    let count: Option<u64> = tenant.query_first("SELECT COUNT(*) FROM records").unwrap();
    assert_eq!(count, Some(0));

    tenant.exec_drop("SET time_zone = ?", ("+02:00",)).unwrap();
    let time_zone: Option<String> = tenant.query_first("SELECT @@time_zone").unwrap();
    assert_eq!(time_zone.as_deref(), Some("+02:00"));
    let other_time_zone: Option<String> = admin.query_first("SELECT @@time_zone").unwrap();
    assert_eq!(other_time_zone.as_deref(), Some("+00:00"));
    tenant.exec_drop("SET autocommit = ?", (0,)).unwrap();
    tenant
        .exec_drop("INSERT INTO records (id) VALUES (?)", (2,))
        .unwrap();
    drop(tenant);
    let mut tenant = server.connect("tenant", "secret", "shard_a").unwrap();
    let count: Option<u64> = tenant.query_first("SELECT COUNT(*) FROM records").unwrap();
    assert_eq!(count, Some(0));
    tenant.query_drop("BEGIN").unwrap();
    tenant
        .exec_drop("INSERT INTO records (id) VALUES (?)", (3,))
        .unwrap();
    tenant.query_drop("COMMIT").unwrap();
    let count: Option<u64> = admin.query_first("SELECT COUNT(*) FROM records").unwrap();
    assert_eq!(count, Some(1));
}

#[test]
fn callback_authentication_assigns_scopes_without_catalog_accounts() {
    let engine = Arc::new(Engine::new(EngineConfig::mysql_strict()));
    engine
        .execute_sql("CREATE TABLE directory_records (id INT PRIMARY KEY)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO directory_records VALUES (1)")
        .unwrap();
    let server =
        Server::start_with_authentication(engine, Authentication::callback(DirectoryAuthenticator));

    assert!(server.connect("directory_reader", "wrong", "app").is_err());
    let mut reader = server
        .connect("directory_reader", "directory-secret", "app")
        .unwrap();
    let count: Option<u64> = reader
        .query_first("SELECT COUNT(*) FROM directory_records")
        .unwrap();
    assert_eq!(count, Some(1));
    assert!(
        reader
            .query_drop("INSERT INTO directory_records VALUES (2)")
            .is_err()
    );
}

#[test]
fn static_users_authenticate_and_enforce_scopes() {
    let engine = Arc::new(Engine::new(EngineConfig::mysql_strict()));
    engine
        .execute_sql("CREATE TABLE static_records (id INT PRIMARY KEY)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO static_records VALUES (1)")
        .unwrap();
    let server = Server::start_with_authentication(
        engine,
        Authentication::static_users([StaticUser::new(
            "static_reader",
            "static-secret",
            [AuthScope::database("app", [AuthPrivilege::Select])],
        )]),
    );

    assert!(server.connect("static_reader", "wrong", "app").is_err());
    let mut reader = server
        .connect("static_reader", "static-secret", "app")
        .unwrap();
    let count: Option<u64> = reader
        .query_first("SELECT COUNT(*) FROM static_records")
        .unwrap();
    assert_eq!(count, Some(1));
    assert!(
        reader
            .query_drop("DELETE FROM static_records WHERE id = 1")
            .is_err()
    );
}

#[test]
fn default_wire_authentication_is_permissive() {
    let server = Server::start_with_authentication(
        Arc::new(Engine::new(EngineConfig::mysql_strict())),
        Authentication::default(),
    );
    let mut user = server
        .connect("unconfigured", "any-password", "app")
        .unwrap();
    user.query_drop("CREATE TABLE permissive_records (id INT PRIMARY KEY)")
        .unwrap();
}

#[test]
fn callback_can_handle_account_operations_outside_the_local_catalog_subset() {
    let directory = AccountDirectory::default();
    let server = Server::start_with_authentication(
        Arc::new(Engine::new(EngineConfig::mysql_strict())),
        Authentication::callback(directory.clone()),
    );
    let mut admin = server
        .connect("directory_admin", "directory-secret", "app")
        .unwrap();

    admin
        .query_drop("ALTER USER 'external_user'@'%' IDENTIFIED BY 'rotated-secret'")
        .unwrap();
    assert_eq!(directory.operations.load(Ordering::Relaxed), 1);
}

fn read_packet(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).unwrap();
    let length = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).unwrap();
    payload
}

fn write_packet(stream: &mut TcpStream, sequence: u8, payload: &[u8]) {
    let length = u32::try_from(payload.len()).unwrap().to_le_bytes();
    stream
        .write_all(&[length[0], length[1], length[2], sequence])
        .unwrap();
    stream.write_all(payload).unwrap();
}

fn query_status(stream: &mut TcpStream, query: &str) -> u16 {
    let mut payload = vec![3];
    payload.extend_from_slice(query.as_bytes());
    write_packet(stream, 0, &payload);
    let response = read_packet(stream);
    assert_eq!(response[0], 0, "expected OK packet: {response:?}");
    // These control statements affect no rows and produce no insert id.
    assert_eq!(&response[1..3], &[0, 0]);
    u16::from_le_bytes([response[3], response[4]])
}

#[test]
fn protocol_ok_packets_report_transaction_and_autocommit_status() {
    let server = Server::start();
    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let greeting = read_packet(&mut stream);
    let version_end = greeting[1..].iter().position(|byte| *byte == 0).unwrap() + 1;
    // Server version NUL, connection id, challenge, filler, capabilities, charset.
    let status_offset = version_end + 1 + 4 + 8 + 1 + 2 + 1;
    assert_eq!(
        u16::from_le_bytes([greeting[status_offset], greeting[status_offset + 1]]) & 3,
        2
    );
    let capabilities = 0x0000_8200u32; // protocol 4.1 and secure connection
    let mut handshake = capabilities.to_le_bytes().to_vec();
    handshake.extend_from_slice(&0u32.to_le_bytes());
    handshake.push(33);
    handshake.extend_from_slice(&[0; 23]);
    handshake.extend_from_slice(b"root\0\0");
    write_packet(&mut stream, 1, &handshake);
    assert_eq!(read_packet(&mut stream)[0], 0);
    assert_eq!(query_status(&mut stream, "BEGIN") & 3, 3);
    for command in [31, 17] {
        // COM_RESET_CONNECTION and COM_CHANGE_USER
        write_packet(&mut stream, 0, &[command]);
        let response = read_packet(&mut stream);
        assert_eq!(response[0], 0xff, "unsupported reset must return ERR");
        assert_eq!(u16::from_le_bytes([response[1], response[2]]), 1047);
        write_packet(&mut stream, 0, &[14]); // COM_PING still reports the active transaction
        let response = read_packet(&mut stream);
        assert_eq!(response[0], 0);
        assert_eq!(u16::from_le_bytes([response[3], response[4]]) & 3, 3);
    }
    assert_eq!(query_status(&mut stream, "ROLLBACK") & 3, 2);
    assert_eq!(query_status(&mut stream, "SET autocommit = 0") & 3, 0);
    assert_eq!(query_status(&mut stream, "BEGIN") & 3, 1);
    assert_eq!(query_status(&mut stream, "COMMIT") & 3, 0);
    assert_eq!(query_status(&mut stream, "SET autocommit = 1") & 3, 2);
}

#[test]
fn compatibility_settings_and_window_metadata_remain_connection_owned() {
    let server = Server::start();
    let mut first = server.connect("root", "", "app").unwrap();
    let mut second = server.connect("root", "", "app").unwrap();
    first
        .query_drop("SET optimizer_switch='semijoin=off'")
        .unwrap();
    assert_eq!(
        first
            .query_first::<String, _>("SELECT @@optimizer_switch")
            .unwrap()
            .unwrap(),
        "semijoin=off"
    );
    assert_ne!(
        second
            .query_first::<String, _>("SELECT @@optimizer_switch")
            .unwrap()
            .unwrap_or_default(),
        "semijoin=off"
    );
    first.query_drop("SET NAMES latin1").unwrap();
    assert_eq!(
        first
            .query_first::<String, _>("SELECT @@character_set_client")
            .unwrap()
            .unwrap(),
        "latin1"
    );
    assert_ne!(
        second
            .query_first::<String, _>("SELECT @@character_set_client")
            .unwrap()
            .unwrap(),
        "latin1"
    );
    first
        .query_drop("CREATE TABLE samples (id INT, n INT, label CHAR(10))")
        .unwrap();
    first
        .query_drop("INSERT INTO samples VALUES (1, 1, 'one'), (2, 2, 'two')")
        .unwrap();
    let values: Vec<(String, String)> = first
        .query("SELECT STD(n) OVER () AS numeric_std, STD(label) OVER () AS text_std FROM samples")
        .unwrap();
    assert_eq!(values, vec![("0.5000".into(), "0".into()); 2]);
    let error = first.query_drop("SELECT STD(n) OVER (ORDER BY id ROWS BETWEEN 0 FOLLOWING AND CURRENT ROW) FROM samples").unwrap_err();
    let mysql::Error::MySqlError(error) = error else {
        panic!("expected server error")
    };
    assert_eq!(error.code, 4014);
    assert_eq!(error.state, "HY000");
}

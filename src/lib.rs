use std::fs;
use std::io::{self, BufRead, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use sqlparser::ast::{SetExpr, Statement, TableFactor};

use crate::sql::engine::{Engine, QueryResult, UniqueMode};

pub mod model;
pub mod schema;
pub mod server;
pub mod sql;
pub mod storage;

pub async fn run_cli() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let (app, command) = parse_cli(&args)?;
    init_tracing(&app.log_filter);

    match command {
        Command::Serve { repl } => {
            if repl {
                app.server.validate()?;
                let engine = server::open_engine(&app.server)?;
                let _server = server::spawn_with_engine(app.server.clone(), engine.clone())?;
                run_repl(&app, engine, true)
            } else {
                server::run(app.server)
            }
        }
        Command::Repl => {
            let engine = server::open_engine(&app.server)?;
            run_repl(&app, engine, false)
        }
        Command::Explain { sql } => print_explain(&sql),
        Command::Help => {
            print_help();
            Ok(())
        }
    }
}

fn init_tracing(filter: &str) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .try_init();
}

#[derive(Debug, Clone)]
struct AppConfig {
    server: server::ServerConfig,
    snapshot_dir: PathBuf,
    log_filter: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: server::ServerConfig::default(),
            snapshot_dir: PathBuf::from(".my-sqweel/snapshots"),
            log_filter: "my_sqweel=info".to_string(),
        }
    }
}

#[derive(Debug)]
enum Command {
    Serve { repl: bool },
    Repl,
    Explain { sql: String },
    Help,
}

#[derive(Debug)]
enum ReplCommand {
    Empty,
    Quit,
    Help,
    Status,
    DriftReport,
    DriftCheck,
    SnapshotSave { name: String },
    SnapshotRestore { name: String },
    SnapshotList,
    IndexRebuildAll,
    IndexRebuildTable { table: String },
    ResetAll,
    ResetTable { table: String },
    Explain { sql: String },
    Sql { sql: String },
}

enum ReplInput {
    Line(io::Result<String>),
    Interrupt,
}

fn parse_cli(args: &[String]) -> Result<(AppConfig, Command)> {
    let mut app = AppConfig::default();
    let mut debug_http = false;
    let mut idx = 0;

    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--" {
            idx += 1;
            break;
        }
        if arg == "-h" || arg == "--help" {
            return Ok((app, Command::Help));
        }
        if let Some(value) = option_value(args, &mut idx, "--bind")? {
            app.server.bind_addr = parse_socket_addr("--bind", &value)?;
            idx += 1;
            continue;
        }
        if let Some(value) = option_value(args, &mut idx, "--data-dir")? {
            app.server.data_dir = non_empty(value);
            idx += 1;
            continue;
        }
        if arg == "--allow-remote" {
            app.server.allow_remote = true;
            idx += 1;
            continue;
        }
        if let Some(value) = option_value(args, &mut idx, "--unique-mode")? {
            app.server.engine.unique_mode = parse_unique_mode(&value)?;
            idx += 1;
            continue;
        }
        if arg == "--debug-http" {
            debug_http = true;
            idx += 1;
            continue;
        }
        if let Some(value) = option_value(args, &mut idx, "--debug-bind")? {
            app.server.debug_addr = Some(parse_socket_addr("--debug-bind", &value)?);
            debug_http = true;
            idx += 1;
            continue;
        }
        if let Some(value) = option_value(args, &mut idx, "--query-delay-ms")? {
            app.server.engine.failure_injection.query_delay_ms =
                parse_u64("--query-delay-ms", &value)?;
            idx += 1;
            continue;
        }
        if let Some(value) = option_value(args, &mut idx, "--fail-read-every")? {
            app.server.engine.failure_injection.fail_read_every =
                parse_u64("--fail-read-every", &value)?;
            idx += 1;
            continue;
        }
        if let Some(value) = option_value(args, &mut idx, "--fail-write-every")? {
            app.server.engine.failure_injection.fail_write_every =
                parse_u64("--fail-write-every", &value)?;
            idx += 1;
            continue;
        }
        if let Some(value) = option_value(args, &mut idx, "--snapshot-dir")? {
            app.snapshot_dir = PathBuf::from(value);
            idx += 1;
            continue;
        }
        if let Some(value) = option_value(args, &mut idx, "--log-filter")? {
            app.log_filter = value;
            idx += 1;
            continue;
        }
        if arg.starts_with('-') {
            return Err(anyhow!("unknown option: {arg}. Run `sqwl help` for usage"));
        }
        break;
    }

    if debug_http && app.server.debug_addr.is_none() {
        app.server.debug_addr = Some(SocketAddr::new(
            app.server.bind_addr.ip(),
            app.server.bind_addr.port().saturating_add(100),
        ));
    }

    let command = parse_command(&args[idx..])?;
    Ok((app, command))
}

fn option_value(args: &[String], idx: &mut usize, name: &str) -> Result<Option<String>> {
    let arg = &args[*idx];
    if arg == name {
        *idx += 1;
        return args
            .get(*idx)
            .cloned()
            .map(Some)
            .ok_or_else(|| anyhow!("{name} requires a value"));
    }

    let prefix = format!("{name}=");
    Ok(arg
        .strip_prefix(&prefix)
        .map(|value| value.trim().to_string()))
}

fn parse_socket_addr(flag: &str, raw: &str) -> Result<SocketAddr> {
    raw.parse()
        .map_err(|err| anyhow!("invalid {flag}={raw:?}: {err}"))
}

fn parse_u64(flag: &str, raw: &str) -> Result<u64> {
    raw.parse::<u64>()
        .map_err(|err| anyhow!("invalid {flag}={raw:?}: {err}"))
}

fn parse_unique_mode(raw: &str) -> Result<UniqueMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "overwrite" => Ok(UniqueMode::Overwrite),
        "enforce" => Ok(UniqueMode::Enforce),
        other => Err(anyhow!(
            "invalid --unique-mode={other:?}; expected overwrite or enforce"
        )),
    }
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn parse_command(args: &[String]) -> Result<Command> {
    if args.is_empty() {
        return Ok(Command::Serve { repl: false });
    }

    match args[0].as_str() {
        "serve" => match args.get(1..).unwrap_or_default() {
            [] => Ok(Command::Serve { repl: false }),
            [flag] if flag == "--repl" => Ok(Command::Serve { repl: true }),
            _ => Err(anyhow!("usage: sqwl [options] serve [--repl]")),
        },
        "repl" => {
            ensure_no_extra(args, "usage: sqwl [options] repl")?;
            Ok(Command::Repl)
        }
        "explain" => {
            let sql = args.get(1..).unwrap_or_default().join(" ");
            if sql.trim().is_empty() {
                return Err(anyhow!("usage: sqwl explain <sql>"));
            }
            Ok(Command::Explain { sql })
        }
        "help" | "-h" | "--help" => {
            ensure_no_extra(args, "usage: sqwl [options] help")?;
            Ok(Command::Help)
        }
        "status" | "drift" | "snapshot" | "index" | "reset" => Err(anyhow!(
            "standalone maintenance command `{}` was removed; use `sqwl repl` or `sqwl serve --repl`",
            args[0]
        )),
        other => Err(anyhow!(
            "unknown command: {other}. Run `sqwl help` for usage"
        )),
    }
}

fn ensure_no_extra(args: &[String], usage: &str) -> Result<()> {
    if args.len() > 1 {
        return Err(anyhow!("{usage}"));
    }
    Ok(())
}

fn run_repl(app: &AppConfig, engine: Arc<Engine>, server_running: bool) -> Result<()> {
    println!("MySqweel maintenance REPL. Type `help` for commands, `quit` to exit.");
    let input = repl_input_events();
    loop {
        print!("sqwl> ");
        io::stdout().flush().context("flush REPL prompt")?;

        let line = match input.recv() {
            Ok(ReplInput::Line(Ok(line))) if line.is_empty() => {
                println!();
                return Ok(());
            }
            Ok(ReplInput::Line(Ok(line))) => line,
            Ok(ReplInput::Line(Err(err))) => return Err(err).context("read REPL input"),
            Ok(ReplInput::Interrupt) => {
                println!();
                return Ok(());
            }
            Err(_) => return Ok(()),
        };

        match parse_repl_command(&line) {
            Ok(ReplCommand::Empty) => continue,
            Ok(ReplCommand::Quit) => return Ok(()),
            Ok(ReplCommand::Help) => print_repl_help(),
            Ok(command) => {
                if let Err(err) = run_repl_command(app, &engine, server_running, command) {
                    eprintln!("error: {err:#}");
                }
            }
            Err(err) => eprintln!("error: {err:#}"),
        }
    }
}

fn repl_input_events() -> mpsc::Receiver<ReplInput> {
    let (tx, rx) = mpsc::channel();

    let stdin_tx = tx.clone();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        loop {
            let mut line = String::new();
            match stdin.read_line(&mut line) {
                Ok(0) => {
                    let _ = stdin_tx.send(ReplInput::Line(Ok(String::new())));
                    return;
                }
                Ok(_) => {
                    if stdin_tx.send(ReplInput::Line(Ok(line))).is_err() {
                        return;
                    }
                }
                Err(err) => {
                    let _ = stdin_tx.send(ReplInput::Line(Err(err)));
                    return;
                }
            }
        }
    });

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = tx.send(ReplInput::Interrupt);
            }
        });
    } else {
        std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            if rt.block_on(tokio::signal::ctrl_c()).is_ok() {
                let _ = tx.send(ReplInput::Interrupt);
            }
        });
    }

    rx
}

fn parse_repl_command(line: &str) -> Result<ReplCommand> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(ReplCommand::Empty);
    }

    let parts = line.split_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        ["quit"] | ["exit"] => Ok(ReplCommand::Quit),
        ["help"] | ["?"] => Ok(ReplCommand::Help),
        ["status"] => Ok(ReplCommand::Status),
        ["drift"] | ["drift", "report"] => Ok(ReplCommand::DriftReport),
        ["drift", "check"] => Ok(ReplCommand::DriftCheck),
        ["snapshot", "save", name] => Ok(ReplCommand::SnapshotSave {
            name: (*name).to_string(),
        }),
        ["snapshot", "restore", name] => Ok(ReplCommand::SnapshotRestore {
            name: (*name).to_string(),
        }),
        ["snapshot", "list"] => Ok(ReplCommand::SnapshotList),
        ["index", "rebuild"] | ["index", "rebuild", "--all"] => Ok(ReplCommand::IndexRebuildAll),
        ["index", "rebuild", table] => Ok(ReplCommand::IndexRebuildTable {
            table: (*table).to_string(),
        }),
        ["reset"] => Ok(ReplCommand::ResetAll),
        ["reset", table] => Ok(ReplCommand::ResetTable {
            table: (*table).to_string(),
        }),
        _ if line.starts_with("explain ") => Ok(ReplCommand::Explain {
            sql: line["explain ".len()..].trim().to_string(),
        }),
        _ if line.starts_with("sql ") => Ok(ReplCommand::Sql {
            sql: line["sql ".len()..].trim().to_string(),
        }),
        _ => Err(anyhow!(
            "unknown REPL command: {line}. Type `help` for commands"
        )),
    }
}

fn run_repl_command(
    app: &AppConfig,
    engine: &Engine,
    server_running: bool,
    command: ReplCommand,
) -> Result<()> {
    match command {
        ReplCommand::Status => print_json(&status_json(app, engine, server_running)),
        ReplCommand::DriftReport => {
            let report = engine.drift_report();
            print_json(&report)
        }
        ReplCommand::DriftCheck => {
            let report = engine.drift_report();
            let issues = drift_issue_count(&report);
            print_json(&json!({
                "ok": issues == 0,
                "issueCount": issues,
                "report": report,
            }))
        }
        ReplCommand::SnapshotSave { name } => {
            let snapshot = engine.snapshot();
            let path = snapshot_file_path(&app.snapshot_dir, &name)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create snapshot directory: {}", parent.display()))?;
            }
            fs::write(
                &path,
                serde_json::to_vec_pretty(&snapshot).context("serialize snapshot")?,
            )
            .with_context(|| format!("write snapshot: {}", path.display()))?;
            print_json(&json!({ "saved": true, "name": name, "path": path.display().to_string() }))
        }
        ReplCommand::SnapshotRestore { name } => {
            let path = snapshot_file_path(&app.snapshot_dir, &name)?;
            let bytes =
                fs::read(&path).with_context(|| format!("read snapshot: {}", path.display()))?;
            let snapshot = serde_json::from_slice(&bytes).context("parse snapshot file")?;
            engine.restore_snapshot(snapshot);
            print_json(
                &json!({ "restored": true, "name": name, "path": path.display().to_string() }),
            )
        }
        ReplCommand::SnapshotList => {
            let names = snapshot_names(&app.snapshot_dir)?;
            print_json(&json!({
                "snapshotDir": app.snapshot_dir.display().to_string(),
                "snapshots": names,
            }))
        }
        ReplCommand::IndexRebuildAll => {
            engine.rebuild_indexes_for_all_tables();
            print_json(&json!({ "rebuilt": true, "scope": "all" }))
        }
        ReplCommand::IndexRebuildTable { table } => {
            engine.rebuild_indexes_for_table(&table)?;
            print_json(&json!({ "rebuilt": true, "scope": "table", "table": table }))
        }
        ReplCommand::ResetAll => {
            engine.reset_all_rows()?;
            print_json(&json!({ "reset": true, "scope": "all" }))
        }
        ReplCommand::ResetTable { table } => {
            engine.reset_table_rows(&table)?;
            print_json(&json!({ "reset": true, "scope": "table", "table": table }))
        }
        ReplCommand::Explain { sql } => print_explain(&sql),
        ReplCommand::Sql { sql } => {
            let results = engine.execute_sql(&sql)?;
            let results = results.iter().map(query_result_json).collect::<Vec<_>>();
            print_json(&json!({ "results": results }))
        }
        ReplCommand::Empty | ReplCommand::Quit | ReplCommand::Help => Ok(()),
    }
}

fn status_json(app: &AppConfig, engine: &Engine, server_running: bool) -> Value {
    let snapshot = engine.snapshot();
    let row_count = snapshot.rows.values().map(|rows| rows.len()).sum::<usize>();
    json!({
        "name": "MySqweel",
        "mode": "development-only",
        "server": {
            "running": server_running,
            "bind": app.server.bind_addr.to_string(),
            "allowRemote": app.server.allow_remote,
            "debugBind": app.server.debug_addr.map(|addr| addr.to_string()),
        },
        "storage": {
            "mode": if app.server.data_dir.is_some() { "directory" } else { "memory" },
            "dataDir": &app.server.data_dir,
        },
        "snapshotDir": app.snapshot_dir.display().to_string(),
        "logFilter": app.log_filter,
        "engine": {
            "uniqueMode": app.server.engine.unique_mode.as_str(),
            "failureInjection": {
                "queryDelayMs": app.server.engine.failure_injection.query_delay_ms,
                "failReadEvery": app.server.engine.failure_injection.fail_read_every,
                "failWriteEvery": app.server.engine.failure_injection.fail_write_every,
            }
        },
        "data": {
            "tables": snapshot.schemas.len(),
            "rows": row_count,
        }
    })
}

fn query_result_json(result: &QueryResult) -> Value {
    json!({
        "rowsAffected": result.rows_affected,
        "lastInsertId": result.last_insert_id,
        "columns": &result.columns,
        "rows": &result.rows,
    })
}

fn snapshot_names(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    if dir.exists() {
        for entry in
            fs::read_dir(dir).with_context(|| format!("read snapshot dir: {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("json")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
            {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

fn snapshot_file_path(snapshot_dir: &Path, name: &str) -> Result<PathBuf> {
    if name.contains('/') || name.contains('\\') {
        return Err(anyhow!("snapshot name must not contain path separators"));
    }
    Ok(snapshot_dir.join(format!("{name}.json")))
}

fn drift_issue_count(report: &Value) -> usize {
    let Some(tables) = report.get("tables").and_then(Value::as_object) else {
        return 0;
    };

    let mut issues = 0usize;
    for entry in tables.values() {
        let Some(table) = entry.as_object() else {
            continue;
        };
        issues += non_empty_len(table.get("missingColumns"));
        issues += non_empty_len(table.get("extraColumns"));
        issues += non_empty_len(table.get("uniqueDuplicates"));
    }
    issues
}

fn non_empty_len(value: Option<&Value>) -> usize {
    match value {
        Some(Value::Array(items)) => items.len(),
        Some(Value::Object(map)) => map.len(),
        _ => 0,
    }
}

fn print_explain(sql: &str) -> Result<()> {
    let statements = sql::parse(sql).context("parse explain input SQL")?;
    let details = statements
        .iter()
        .map(|statement| {
            json!({
                "kind": statement_kind(statement),
                "tables": statement_tables(statement),
                "normalized": statement.to_string(),
            })
        })
        .collect::<Vec<_>>();
    print_json(&json!({ "count": details.len(), "statements": details }))
}

fn statement_kind(statement: &Statement) -> &'static str {
    match statement {
        Statement::Query(_) => "query",
        Statement::Insert(_) => "insert",
        Statement::Update { .. } => "update",
        Statement::Delete(_) => "delete",
        Statement::CreateTable(_) => "create_table",
        Statement::AlterTable { .. } => "alter_table",
        Statement::Drop { .. } => "drop",
        Statement::CreateIndex(_) => "create_index",
        Statement::Truncate { .. } => "truncate",
        _ => "other",
    }
}

fn statement_tables(statement: &Statement) -> Vec<String> {
    match statement {
        Statement::Query(query) => {
            let SetExpr::Select(select) = query.body.as_ref() else {
                return Vec::new();
            };
            let mut tables = Vec::new();
            for from in &select.from {
                if let Some(name) = table_factor_name(&from.relation) {
                    tables.push(name);
                }
                for join in &from.joins {
                    if let Some(name) = table_factor_name(&join.relation) {
                        tables.push(name);
                    }
                }
            }
            tables
        }
        Statement::Insert(insert) => vec![insert.table_name.to_string()],
        Statement::Update { table, .. } => vec![table.to_string()],
        Statement::Delete(delete) => delete_tables(delete),
        Statement::CreateTable(create) => vec![create.name.to_string()],
        Statement::AlterTable { name, .. } => vec![name.to_string()],
        Statement::Truncate { table_names, .. } => table_names
            .iter()
            .map(|target| target.name.to_string())
            .collect(),
        _ => Vec::new(),
    }
}

fn table_factor_name(relation: &TableFactor) -> Option<String> {
    match relation {
        TableFactor::Table { name, .. } => Some(name.to_string()),
        _ => None,
    }
}

fn delete_tables(delete: &sqlparser::ast::Delete) -> Vec<String> {
    match &delete.from {
        sqlparser::ast::FromTable::WithFromKeyword(tables)
        | sqlparser::ast::FromTable::WithoutKeyword(tables) => tables
            .iter()
            .filter_map(|table| table_factor_name(&table.relation))
            .collect(),
    }
}

fn print_help() {
    println!(
        "MySqweel usage:\n  sqwl [options] serve [--repl]\n  sqwl [options] repl\n  sqwl explain <sql>\n\nOptions:\n  --bind <addr>                 MySQL bind address (default 127.0.0.1:3307)\n  --data-dir <dir>              locked Lux-backed data directory\n  --allow-remote                allow non-loopback bind addresses\n  --unique-mode <mode>          overwrite or enforce (default overwrite)\n  --debug-http                  enable debug HTTP endpoints\n  --debug-bind <addr>           debug HTTP bind address (enables debug HTTP)\n  --query-delay-ms <n>          add fixed latency per SQL statement\n  --fail-read-every <n>         fail every Nth read statement\n  --fail-write-every <n>        fail every Nth write statement\n  --snapshot-dir <path>         REPL snapshot directory (default .my-sqweel/snapshots)\n  --log-filter <filter>         tracing filter (default my_sqweel=info)\n\nMaintenance commands run inside `sqwl repl` or `sqwl serve --repl`."
    );
}

fn print_repl_help() {
    println!(
        "REPL commands:\n  status\n  drift report\n  drift check\n  snapshot save <name>\n  snapshot restore <name>\n  snapshot list\n  index rebuild [--all|<table>]\n  reset [table]\n  explain <sql>\n  sql <sql>\n  help\n  quit\n\nExit shortcuts:\n  Ctrl+C\n  Ctrl+D"
    );
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cli_global_options_before_command() {
        let args = vec![
            "--bind".to_string(),
            "127.0.0.1:3310".to_string(),
            "--data-dir=./data".to_string(),
            "--unique-mode".to_string(),
            "enforce".to_string(),
            "--debug-http".to_string(),
            "--snapshot-dir".to_string(),
            "./snapshots".to_string(),
            "--log-filter=my_sqweel=debug".to_string(),
            "serve".to_string(),
            "--repl".to_string(),
        ];

        let (app, command) = parse_cli(&args).unwrap();
        assert!(matches!(command, Command::Serve { repl: true }));
        assert_eq!(app.server.bind_addr.to_string(), "127.0.0.1:3310");
        assert_eq!(app.server.data_dir.as_deref(), Some("./data"));
        assert_eq!(app.server.engine.unique_mode, UniqueMode::Enforce);
        assert_eq!(
            app.server
                .debug_addr
                .map(|addr| addr.to_string())
                .as_deref(),
            Some("127.0.0.1:3410")
        );
        assert_eq!(app.snapshot_dir, PathBuf::from("./snapshots"));
        assert_eq!(app.log_filter, "my_sqweel=debug");
    }

    #[test]
    fn parse_cli_allows_only_two_unique_modes() {
        let err = parse_cli(&["--unique-mode".to_string(), "warn".to_string()]).unwrap_err();
        assert!(err.to_string().contains("expected overwrite or enforce"));
    }

    #[test]
    fn parse_cli_removes_standalone_maintenance_commands() {
        let err = parse_cli(&["status".to_string()]).unwrap_err();
        assert!(err.to_string().contains("standalone maintenance command"));
    }

    #[test]
    fn parse_repl_maintenance_commands() {
        assert!(matches!(
            parse_repl_command("status").unwrap(),
            ReplCommand::Status
        ));
        assert!(parse_repl_command("serve").is_err());
        assert!(matches!(
            parse_repl_command("index rebuild users").unwrap(),
            ReplCommand::IndexRebuildTable { table } if table == "users"
        ));
        assert!(matches!(
            parse_repl_command("sql SELECT 1").unwrap(),
            ReplCommand::Sql { sql } if sql == "SELECT 1"
        ));
    }
}

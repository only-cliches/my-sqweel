//! The deliberately small MySQL account/database boundary used by embedded applications.
//! Accounts grant DML on whole databases; administrative rights cannot be granted over SQL.
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use sqlparser::ast::{
    AlterTableOperation, ColumnDef, ColumnOption, Expr, Ident, ObjectName, Query, SetExpr,
    Statement, TableConstraint, VisitMut, VisitorMut,
};
use sqlparser::dialect::{Dialect, MySqlDialect};
use sqlparser::tokenizer::{Token, Tokenizer};

#[derive(Debug, Clone)]
pub(crate) struct Identity {
    pub(crate) username: String,
    incarnation: String,
}
impl Identity {
    pub(crate) fn unauthenticated() -> Self {
        Self {
            username: String::new(),
            incarnation: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum Privilege {
    Select,
    Insert,
    Update,
    Delete,
}

#[derive(Clone, Serialize, Deserialize)]
struct Account {
    incarnation: String,
    // Store SHA1(SHA1(password)), never the clear-text password or reusable first hash.
    password_hash: Option<[u8; 20]>,
    admin: bool,
    grants: BTreeMap<String, BTreeSet<Privilege>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Catalog {
    databases: BTreeSet<String>,
    users: BTreeMap<String, Account>,
}

impl Default for Catalog {
    fn default() -> Self {
        let mut catalog = Self {
            databases: BTreeSet::from(["app".into()]),
            users: BTreeMap::new(),
        };
        catalog.set_admin_credentials("root", "");
        catalog
    }
}

impl Catalog {
    /// Bootstrap only: SQL cannot create another administrator or replace this credential.
    pub(crate) fn set_admin_credentials(&mut self, username: &str, password: &str) {
        self.users.retain(|_, account| !account.admin);
        self.users.insert(
            username.into(),
            Account {
                incarnation: uuid::Uuid::new_v4().to_string(),
                password_hash: password_hash(password),
                admin: true,
                grants: BTreeMap::new(),
            },
        );
    }

    pub(crate) fn authenticate(&self, username: &str, salt: &[u8], response: &[u8]) -> bool {
        let Some(account) = self.users.get(username) else {
            return false;
        };
        let Some(expected) = account.password_hash else {
            return response.is_empty();
        };
        if response.len() != 20 {
            return false;
        }
        let mut challenge = sha1_smol::Sha1::new();
        challenge.update(salt);
        challenge.update(&expected);
        let mask = challenge.digest().bytes();
        let candidate: [u8; 20] = std::array::from_fn(|index| response[index] ^ mask[index]);
        let actual = sha1_smol::Sha1::from(&candidate).digest().bytes();
        actual
            .iter()
            .zip(expected)
            .fold(0u8, |difference, (left, right)| {
                difference | (*left ^ right)
            })
            == 0
    }

    pub(crate) fn administrator_identity(&self) -> Identity {
        self.users
            .iter()
            .find(|(_, account)| account.admin)
            .and_then(|(name, _)| self.identity(name).ok())
            .unwrap_or_else(Identity::unauthenticated)
    }

    pub(crate) fn identity(&self, username: &str) -> Result<Identity> {
        let account = self
            .users
            .get(username)
            .ok_or_else(|| anyhow!("Access denied for user"))?;
        Ok(Identity {
            username: username.into(),
            incarnation: account.incarnation.clone(),
        })
    }

    pub(crate) fn is_admin(&self, identity: &Identity) -> bool {
        self.users
            .get(&identity.username)
            .is_some_and(|account| account.admin && account.incarnation == identity.incarnation)
    }

    pub(crate) fn database_names(&self) -> impl Iterator<Item = &str> {
        self.databases.iter().map(String::as_str)
    }

    pub(crate) fn check_database(&self, identity: &Identity, database: &str) -> Result<()> {
        ensure!(
            self.databases.contains(database),
            "Unknown database '{database}'"
        );
        let account = self
            .users
            .get(&identity.username)
            .ok_or_else(|| anyhow!("Access denied for user"))?;
        ensure!(
            account.incarnation == identity.incarnation,
            "Account was replaced; reconnect to authenticate"
        );
        ensure!(
            account.admin
                || account
                    .grants
                    .get(database)
                    .is_some_and(|grants| !grants.is_empty()),
            "Access denied for database '{database}'"
        );
        Ok(())
    }

    fn check_privilege(
        &self,
        identity: &Identity,
        database: &str,
        privilege: Privilege,
    ) -> Result<()> {
        self.check_database(identity, database)?;
        let account = &self.users[&identity.username];
        ensure!(
            account.admin
                || account
                    .grants
                    .get(database)
                    .is_some_and(|grants| grants.contains(&privilege)),
            "{privilege:?} command denied for database '{database}'"
        );
        Ok(())
    }

    pub(crate) fn apply(
        &mut self,
        identity: &Identity,
        command: AdminCommand,
    ) -> Result<CatalogEffect> {
        ensure!(self.is_admin(identity), "Administrative command denied");
        match command {
            AdminCommand::CreateDatabase {
                name,
                if_not_exists,
            } => {
                if self.databases.contains(&name) {
                    ensure!(if_not_exists, "Database '{name}' already exists");
                    return Ok(CatalogEffect::None);
                }
                self.databases.insert(name.clone());
                Ok(CatalogEffect::CreateDatabase(name))
            }
            AdminCommand::DropDatabase { name, if_exists } => {
                if !self.databases.remove(&name) {
                    ensure!(if_exists, "Unknown database '{name}'");
                    return Ok(CatalogEffect::None);
                }
                for account in self.users.values_mut() {
                    account.grants.remove(&name);
                }
                Ok(CatalogEffect::DropDatabase(name))
            }
            AdminCommand::CreateUser {
                username,
                password,
                if_not_exists,
            } => {
                if self.users.contains_key(&username) {
                    ensure!(if_not_exists, "User already exists");
                } else {
                    self.users.insert(
                        username,
                        Account {
                            incarnation: uuid::Uuid::new_v4().to_string(),
                            password_hash: password_hash(&password),
                            admin: false,
                            grants: BTreeMap::new(),
                        },
                    );
                }
                Ok(CatalogEffect::None)
            }
            AdminCommand::DropUser {
                username,
                if_exists,
            } => {
                ensure!(
                    !self
                        .users
                        .get(&username)
                        .is_some_and(|account| account.admin),
                    "Cannot drop bootstrap administrator"
                );
                ensure!(
                    self.users.remove(&username).is_some() || if_exists,
                    "Unknown user"
                );
                Ok(CatalogEffect::None)
            }
            AdminCommand::Grant {
                username,
                database,
                privileges,
            } => {
                ensure!(
                    self.databases.contains(&database),
                    "Unknown database '{database}'"
                );
                let account = self
                    .users
                    .get_mut(&username)
                    .ok_or_else(|| anyhow!("Unknown user"))?;
                ensure!(
                    !account.admin,
                    "Cannot modify bootstrap administrator grants"
                );
                account
                    .grants
                    .entry(database)
                    .or_default()
                    .extend(privileges);
                Ok(CatalogEffect::None)
            }
            AdminCommand::RevokeAll { username } => {
                let account = self
                    .users
                    .get_mut(&username)
                    .ok_or_else(|| anyhow!("Unknown user"))?;
                ensure!(
                    !account.admin,
                    "Cannot modify bootstrap administrator grants"
                );
                account.grants.clear();
                Ok(CatalogEffect::None)
            }
        }
    }

    fn check_statement(
        &self,
        identity: &Identity,
        database: &str,
        statement: &Statement,
    ) -> Result<()> {
        let returns_rows = match statement {
            Statement::Insert(insert) => insert.returning.is_some(),
            Statement::Update { returning, .. } => returning.is_some(),
            Statement::Delete(delete) => delete.returning.is_some(),
            _ => false,
        };
        if returns_rows {
            self.check_privilege(identity, database, Privilege::Select)?;
        }
        let required = match statement {
            Statement::Query(_) => Some(Privilege::Select),
            Statement::Insert(insert) => {
                if insert.on.is_some() {
                    self.check_privilege(identity, database, Privilege::Update)?;
                }
                if insert.replace_into {
                    self.check_privilege(identity, database, Privilege::Delete)?;
                }
                Some(Privilege::Insert)
            }
            Statement::Update { .. } => Some(Privilege::Update),
            Statement::Delete(_) => Some(Privilege::Delete),
            Statement::ShowTables { .. }
            | Statement::ShowColumns { .. }
            | Statement::ShowCreate { .. } => Some(Privilege::Select),
            Statement::ShowVariable { variable }
                if variable.first().is_some_and(|name| {
                    matches!(
                        name.value.to_ascii_uppercase().as_str(),
                        "INDEX" | "INDEXES" | "KEYS"
                    )
                }) =>
            {
                Some(Privilege::Select)
            }
            Statement::Savepoint { .. }
            | Statement::ReleaseSavepoint { .. }
            | Statement::StartTransaction { .. }
            | Statement::Commit { .. }
            | Statement::Rollback { .. }
            | Statement::SetTransaction { .. }
            | Statement::SetVariable { .. }
            | Statement::SetNames { .. }
            | Statement::SetNamesDefault { .. }
            | Statement::ShowVariable { .. }
            | Statement::ShowVariables { .. }
            | Statement::ShowStatus { .. } => None,
            _ => {
                ensure!(
                    self.is_admin(identity),
                    "Statement requires database administration privileges"
                );
                None
            }
        };
        if let Some(privilege) = required {
            self.check_privilege(identity, database, privilege)?;
        }
        Ok(())
    }

    pub(crate) fn validate_prepared(
        &self,
        identity: &Identity,
        database: &str,
        sql: &str,
    ) -> Result<String> {
        let normalized = self.authorize_and_normalize(identity, database, sql)?;
        let statements = parse_session_statement(&normalized)?;
        ensure!(
            matches!(
                statements.first(),
                Some(
                    Statement::Query(_)
                        | Statement::Insert(_)
                        | Statement::Update { .. }
                        | Statement::Delete(_)
                )
            ),
            "SQL PREPARE supports one SELECT, INSERT, UPDATE or DELETE statement"
        );
        Ok(normalized)
    }

    /// Parse before execution and visit every nested relation. A connection targets one
    /// physical engine; rejecting cross-database SQL prevents accidental name truncation
    /// in the executor from turning another database's table into a local table.
    pub(crate) fn authorize_and_normalize(
        &self,
        identity: &Identity,
        database: &str,
        sql: &str,
    ) -> Result<String> {
        self.check_database(identity, database)?;
        if let Some((mut table, index, if_exists)) = parse_index_drop(sql)? {
            ensure!(self.is_admin(identity), "Administrative command denied");
            DatabaseVisitor {
                catalog: self,
                identity,
                database,
                read_only: false,
                rewritten: false,
            }
            .relation(&mut table)?;
            let conditional = if if_exists { "IF EXISTS " } else { "" };
            return Ok(format!(
                "ALTER TABLE {table} DROP INDEX {conditional}{index}"
            ));
        }
        if let Some((mut table, next)) = parse_auto_increment_option(sql)? {
            ensure!(self.is_admin(identity), "Administrative command denied");
            DatabaseVisitor {
                catalog: self,
                identity,
                database,
                read_only: false,
                rewritten: false,
            }
            .relation(&mut table)?;
            return Ok(format!("ALTER TABLE {table} AUTO_INCREMENT={next}"));
        }
        let renamed = normalize_rename(sql)?;
        let mut statements = parse_session_statement(renamed.as_deref().unwrap_or(sql))?;
        ensure!(
            statements.len() == 1,
            "Exactly one SQL statement is required"
        );
        let statement = &mut statements[0];
        let mut visitor = DatabaseVisitor {
            catalog: self,
            identity,
            database,
            read_only: matches!(statement, Statement::Query(_)),
            rewritten: false,
        };
        if let ControlFlow::Break(error) = statement.visit(&mut visitor) {
            return Err(error);
        }
        if !visitor.rewritten {
            return Ok(renamed.unwrap_or_else(|| sql.to_owned()));
        }
        // sqlparser's generic formatter escapes quotes, but not MySQL backslashes.
        // Re-encoding a qualified statement must not decode JSON escapes twice.
        let _ = statement.visit(&mut MysqlStringEscapes);
        let formatted = statement.to_string();
        let upper = sql.trim_start().to_ascii_uppercase();
        if upper.starts_with("CHECK TABLE ") {
            return Ok(formatted.replacen("ANALYZE TABLE", "CHECK TABLE", 1));
        }
        if upper.starts_with("CREATE OR REPLACE INDEX ")
            || upper.starts_with("CREATE OR REPLACE KEY ")
        {
            return Ok(formatted.replacen("CREATE INDEX", "CREATE OR REPLACE INDEX", 1));
        }
        if upper.starts_with("ALTER TABLE ")
            && upper
                .trim_end_matches(';')
                .trim_end()
                .ends_with(", DISABLE KEYS")
        {
            return Ok(format!("{formatted}, DISABLE KEYS"));
        }
        Ok(formatted)
    }
}

/// Parse compatibility syntax into a complete AST for authorization and transaction
/// classification. Execution retains the original SQL so rewrites do not erase
/// warning context or change CHECK/REPLACE semantics.
pub(super) fn parse_session_statement(sql: &str) -> Result<Vec<Statement>> {
    if let Some((table, next)) = parse_auto_increment_option(sql)? {
        return Ok(crate::sql::parse(&format!(
            "ALTER TABLE {table} SET TBLPROPERTIES ('auto_increment' = '{next}')"
        ))?);
    }

    if let Ok(statements) = crate::sql::parse(sql) {
        return Ok(statements);
    }
    let text = sql.trim().trim_end_matches(';').trim();
    let upper = text.to_ascii_uppercase();
    for prefix in [
        "CREATE VIEW IF NOT EXISTS ",
        "CREATE OR REPLACE VIEW IF NOT EXISTS ",
    ] {
        if upper.starts_with(prefix) {
            let base = format!(
                "{}{}",
                prefix.replace("IF NOT EXISTS ", ""),
                &text[prefix.len()..]
            );
            let mut statements = crate::sql::parse(&base)?;
            if let [Statement::CreateView { if_not_exists, .. }] = statements.as_mut_slice() {
                *if_not_exists = true;
                return Ok(statements);
            }
            bail!("Expected one CREATE VIEW statement");
        }
    }
    let rewritten = if upper.starts_with("ALTER TABLE ") && upper.ends_with(", DISABLE KEYS") {
        let base = &text[..text.len() - ", DISABLE KEYS".len()];
        let statements = crate::sql::parse(base)?;
        ensure!(
            matches!(statements.as_slice(), [Statement::AlterTable { operations, .. }]
            if matches!(operations.as_slice(), [AlterTableOperation::RenameTable { .. }])),
            "DISABLE KEYS compatibility requires a single table rename"
        );
        return Ok(statements);
    } else if upper.starts_with("CHECK TABLE ") {
        format!("ANALYZE TABLE {}", &text["CHECK TABLE ".len()..])
    } else if upper.starts_with("CREATE OR REPLACE INDEX ") {
        format!("CREATE INDEX {}", &text["CREATE OR REPLACE INDEX ".len()..])
    } else if upper.starts_with("CREATE OR REPLACE KEY ") {
        format!("CREATE INDEX {}", &text["CREATE OR REPLACE KEY ".len()..])
    } else if upper.starts_with("EXPLAIN FORMAT=JSON ") {
        format!(
            "EXPLAIN FORMAT JSON {}",
            &text["EXPLAIN FORMAT=JSON ".len()..]
        )
    } else if (upper.starts_with("INSERT ") || upper.starts_with("REPLACE "))
        && upper.ends_with(" RETURNING *")
    {
        let base = &text[..text.len() - " RETURNING *".len()];
        let mut statements = crate::sql::parse(base)?;
        ensure!(
            statements.len() == 1,
            "Exactly one SQL statement is required"
        );
        if let Statement::Insert(insert) = &mut statements[0] {
            insert.returning = Some(vec![sqlparser::ast::SelectItem::Wildcard(
                Default::default(),
            )]);
            return Ok(statements);
        }
        bail!("RETURNING requires INSERT or REPLACE");
    } else {
        text.to_owned()
    };
    Ok(crate::sql::parse(&rewritten)?)
}

// Parse the complete option, including its table, before authorization. Never
// discard the option and leave an incomplete ALTER TABLE for the generic parser.
fn parse_auto_increment_option(sql: &str) -> Result<Option<(ObjectName, u64)>> {
    if !may_start_with(sql, &["ALTER"]) {
        return Ok(None);
    }
    use sqlparser::keywords::Keyword;
    use sqlparser::parser::Parser;
    let mut parser = Parser::new(&MySqlDialect {}).try_with_sql(sql)?;
    if !parser.parse_keywords(&[Keyword::ALTER, Keyword::TABLE]) {
        return Ok(None);
    }
    let table = parser.parse_object_name(false)?;
    if !parser.parse_keyword(Keyword::AUTO_INCREMENT) {
        return Ok(None);
    }
    let _ = parser.consume_token(&Token::Eq);
    let Token::Number(value, _) = parser.next_token().token else {
        bail!("AUTO_INCREMENT requires an unsigned integer");
    };
    let next = value.parse::<u64>()?;
    let _ = parser.consume_token(&Token::SemiColon);
    ensure!(
        parser.peek_token().token == Token::EOF,
        "unsupported trailing AUTO_INCREMENT options"
    );
    Ok(Some((table, next)))
}

struct MysqlStringEscapes;
impl VisitorMut for MysqlStringEscapes {
    type Break = std::convert::Infallible;
    fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<Self::Break> {
        if let Expr::Value(
            sqlparser::ast::Value::SingleQuotedString(value)
            | sqlparser::ast::Value::DoubleQuotedString(value),
        ) = expression
        {
            *value = value.replace('\\', "\\\\");
        }
        ControlFlow::Continue(())
    }
}

struct DatabaseVisitor<'a> {
    catalog: &'a Catalog,
    identity: &'a Identity,
    database: &'a str,
    read_only: bool,
    // Set at each qualifier rewrite instead of cloning/comparing bulk VALUES ASTs.
    rewritten: bool,
}
impl DatabaseVisitor<'_> {
    fn column(&mut self, column: &mut ColumnDef) -> Result<()> {
        for option in &mut column.options {
            if let ColumnOption::ForeignKey { foreign_table, .. } = &mut option.option {
                self.relation(foreign_table)?;
            }
        }
        Ok(())
    }
    fn constraint(&mut self, constraint: &mut TableConstraint) -> Result<()> {
        if let TableConstraint::ForeignKey { foreign_table, .. } = constraint {
            self.relation(foreign_table)?;
        }
        Ok(())
    }
    fn relation(&mut self, name: &mut ObjectName) -> Result<()> {
        match name.0.as_slice() {
            [_] => Ok(()),
            [database, _] if database.value == self.database => {
                name.0.remove(0);
                self.rewritten = true;
                Ok(())
            }
            [database, _]
                if database.value.eq_ignore_ascii_case("information_schema") && self.read_only =>
            {
                Ok(())
            }
            _ => bail!("Cross-database statements are not supported; select the database first"),
        }
    }
}
impl VisitorMut for DatabaseVisitor<'_> {
    type Break = anyhow::Error;
    fn pre_visit_statement(&mut self, statement: &mut Statement) -> ControlFlow<Self::Break> {
        let result = (|| {
            self.catalog
                .check_statement(self.identity, self.database, statement)?;
            // These ObjectName fields have no relation visitor annotations in sqlparser.
            match statement {
                Statement::Delete(delete) => {
                    for table in &mut delete.tables {
                        self.relation(table)?;
                    }
                }
                Statement::Drop { names, .. } => {
                    for name in names {
                        self.relation(name)?;
                    }
                }
                Statement::ShowCreate { obj_name, .. } => self.relation(obj_name)?,
                Statement::ShowTables { show_options, .. } => {
                    if let Some(name) = show_options
                        .show_in
                        .as_ref()
                        .and_then(|clause| clause.parent_name.as_ref())
                    {
                        ensure!(
                            name.0.len() == 1 && name.0[0].value == self.database,
                            "Cross-database metadata reference denied"
                        );
                    }
                }
                Statement::CreateTable(table) => {
                    for name in [&mut table.like, &mut table.clone].into_iter().flatten() {
                        self.relation(name)?;
                    }
                    for column in &mut table.columns {
                        self.column(column)?;
                    }
                    for constraint in &mut table.constraints {
                        self.constraint(constraint)?;
                    }
                }
                Statement::AlterTable { operations, .. } => {
                    for operation in operations {
                        match operation {
                            AlterTableOperation::AddConstraint(constraint) => {
                                self.constraint(constraint)?
                            }
                            AlterTableOperation::AddColumn { column_def, .. } => {
                                self.column(column_def)?
                            }
                            AlterTableOperation::RenameTable { table_name } => {
                                self.relation(table_name)?
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        })();
        match result {
            Ok(()) => ControlFlow::Continue(()),
            Err(error) => ControlFlow::Break(error),
        }
    }
    fn pre_visit_relation(&mut self, relation: &mut ObjectName) -> ControlFlow<Self::Break> {
        match self.relation(relation) {
            Ok(()) => ControlFlow::Continue(()),
            Err(error) => ControlFlow::Break(error),
        }
    }
    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        if matches!(query.body.as_ref(), SetExpr::Values(_)) {
            return ControlFlow::Continue(());
        }
        match self
            .catalog
            .check_privilege(self.identity, self.database, Privilege::Select)
        {
            Ok(()) => ControlFlow::Continue(()),
            Err(error) => ControlFlow::Break(error),
        }
    }
    fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<Self::Break> {
        if let Expr::CompoundIdentifier(parts) = expression {
            if parts.len() >= 3 {
                if parts.len() != 3
                    || (parts[0].value != self.database
                        && !parts[0].value.eq_ignore_ascii_case("information_schema"))
                {
                    return ControlFlow::Break(anyhow!("Cross-database column reference denied"));
                }
                parts.remove(0);
                self.rewritten = true;
            }
        }
        ControlFlow::Continue(())
    }
}

fn password_hash(password: &str) -> Option<[u8; 20]> {
    if password.is_empty() {
        None
    } else {
        Some(
            sha1_smol::Sha1::from(&sha1_smol::Sha1::from(password).digest().bytes())
                .digest()
                .bytes(),
        )
    }
}

pub(crate) enum AdminCommand {
    CreateDatabase {
        name: String,
        if_not_exists: bool,
    },
    DropDatabase {
        name: String,
        if_exists: bool,
    },
    CreateUser {
        username: String,
        password: String,
        if_not_exists: bool,
    },
    DropUser {
        username: String,
        if_exists: bool,
    },
    Grant {
        username: String,
        database: String,
        privileges: BTreeSet<Privilege>,
    },
    RevokeAll {
        username: String,
    },
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CatalogEffect {
    None,
    CreateDatabase(String),
    DropDatabase(String),
}

// Recognizers below previously tokenized an entire bulk INSERT just to discover
// that its first keyword was not theirs. This is only a negative dispatch hint:
// comments, quoted identifiers and other ambiguous prefixes use the full parser.
pub(crate) fn may_start_with(sql: &str, keywords: &[&str]) -> bool {
    let first = sql
        .trim_start()
        .split(|character: char| !character.is_ascii_alphabetic())
        .next()
        .unwrap_or_default();
    first.is_empty()
        || keywords
            .iter()
            .any(|keyword| first.eq_ignore_ascii_case(keyword))
}

impl AdminCommand {
    /// MySQL's user@host and CREATE USER syntax are not supported by sqlparser's AST.
    /// Use its tokenizer, accept only the supported grammar, and reject all trailing tokens.
    pub(crate) fn parse(sql: &str) -> Result<Option<Self>> {
        if !may_start_with(sql, &["CREATE", "DROP", "GRANT", "REVOKE"]) {
            return Ok(None);
        }
        let mut tokens = Tokens::new(sql)?;
        let command = if tokens.take_keyword("CREATE") {
            if tokens.take_keyword("DATABASE") || tokens.take_keyword("SCHEMA") {
                let if_not_exists = tokens.if_not_exists()?;
                let name = tokens.database()?;
                if tokens.take_keyword("DEFAULT")
                    || tokens.peek_keyword("CHARACTER")
                    || tokens.peek_keyword("CHARSET")
                {
                    if tokens.take_keyword("CHARACTER") {
                        tokens.keyword("SET")?;
                    } else {
                        tokens.keyword("CHARSET")?;
                    }
                    tokens.take(&Token::Eq);
                    ensure!(
                        tokens.identifier()?.eq_ignore_ascii_case("utf8mb4"),
                        "Only utf8mb4 databases are supported"
                    );
                }
                if tokens.take_keyword("COLLATE") {
                    tokens.take(&Token::Eq);
                    let collation = tokens.identifier()?;
                    ensure!(
                        matches!(
                            collation.to_ascii_lowercase().as_str(),
                            "utf8mb4_unicode_ci"
                                | "utf8mb4_general_ci"
                                | "utf8mb4_bin"
                                | "utf8mb4_0900_ai_ci"
                        ),
                        "Unsupported database collation"
                    );
                }
                Self::CreateDatabase {
                    name,
                    if_not_exists,
                }
            } else if tokens.take_keyword("USER") {
                let if_not_exists = tokens.if_not_exists()?;
                let username = tokens.account()?;
                tokens.keyword("IDENTIFIED")?;
                tokens.keyword("BY")?;
                let password = tokens.string()?;
                Self::CreateUser {
                    username,
                    password,
                    if_not_exists,
                }
            } else {
                return Ok(None);
            }
        } else if tokens.take_keyword("DROP") {
            if tokens.take_keyword("DATABASE") || tokens.take_keyword("SCHEMA") {
                let if_exists = tokens.if_exists()?;
                Self::DropDatabase {
                    name: tokens.database()?,
                    if_exists,
                }
            } else if tokens.take_keyword("USER") {
                let if_exists = tokens.if_exists()?;
                Self::DropUser {
                    username: tokens.account()?,
                    if_exists,
                }
            } else {
                return Ok(None);
            }
        } else if tokens.take_keyword("GRANT") {
            let mut privileges = BTreeSet::new();
            loop {
                let privilege = match tokens.identifier()?.to_ascii_uppercase().as_str() {
                    "SELECT" => Privilege::Select,
                    "INSERT" => Privilege::Insert,
                    "UPDATE" => Privilege::Update,
                    "DELETE" => Privilege::Delete,
                    _ => bail!("Only SELECT, INSERT, UPDATE and DELETE grants are supported"),
                };
                privileges.insert(privilege);
                if !tokens.take(&Token::Comma) {
                    break;
                }
            }
            tokens.keyword("ON")?;
            let database = tokens.database()?;
            tokens.expect(&Token::Period)?;
            tokens.expect(&Token::Mul)?;
            tokens.keyword("TO")?;
            Self::Grant {
                username: tokens.account()?,
                database,
                privileges,
            }
        } else if tokens.take_keyword("REVOKE") {
            tokens.keyword("ALL")?;
            tokens.keyword("PRIVILEGES")?;
            if tokens.take(&Token::Comma) {
                tokens.keyword("GRANT")?;
                tokens.keyword("OPTION")?;
            }
            tokens.keyword("FROM")?;
            Self::RevokeAll {
                username: tokens.account()?,
            }
        } else {
            return Ok(None);
        };
        tokens.finish()?;
        Ok(Some(command))
    }
}

pub(crate) enum PreparedSource {
    Sql(String),
    Variable(String),
}
pub(crate) enum PreparedCommand {
    Prepare {
        name: String,
        source: PreparedSource,
    },
    Execute {
        name: String,
        variables: Vec<String>,
    },
    Deallocate {
        name: String,
    },
}
impl PreparedCommand {
    pub(crate) fn parse(sql: &str) -> Result<Option<Self>> {
        if !may_start_with(sql, &["PREPARE", "EXECUTE", "DEALLOCATE"]) {
            return Ok(None);
        }
        let mut tokens = Tokens::new(sql)?;
        let command = if tokens.take_keyword("PREPARE") {
            let name = tokens.identifier()?.to_ascii_lowercase();
            tokens.keyword("FROM")?;
            let source = if tokens.take(&Token::AtSign) {
                PreparedSource::Variable(tokens.identifier()?.to_ascii_lowercase())
            } else {
                PreparedSource::Sql(tokens.string()?)
            };
            Self::Prepare { name, source }
        } else if tokens.take_keyword("EXECUTE") {
            let name = tokens.identifier()?.to_ascii_lowercase();
            let mut variables = Vec::new();
            if tokens.take_keyword("USING") {
                loop {
                    tokens.expect(&Token::AtSign)?;
                    variables.push(tokens.identifier()?.to_ascii_lowercase());
                    if !tokens.take(&Token::Comma) {
                        break;
                    }
                }
            }
            Self::Execute { name, variables }
        } else if tokens.take_keyword("DEALLOCATE") {
            tokens.keyword("PREPARE")?;
            Self::Deallocate {
                name: tokens.identifier()?.to_ascii_lowercase(),
            }
        } else {
            return Ok(None);
        };
        tokens.finish()?;
        Ok(Some(command))
    }
}

// sqlparser lacks MySQL DROP INDEX ... ON table. Keep the table identity;
// the legacy parser's fallback discarded it and could drop names in other tables.
pub(super) fn parse_index_drop(sql: &str) -> Result<Option<(ObjectName, Ident, bool)>> {
    if !may_start_with(sql, &["DROP", "ALTER"]) {
        return Ok(None);
    }
    let mut tokens = Tokens::new(sql)?;
    if tokens.take_keyword("DROP") && tokens.take_keyword("INDEX") {
        let if_exists = tokens.if_exists()?;
        let index = Ident::with_quote('`', tokens.identifier()?);
        if !tokens.take_keyword("ON") {
            return Ok(None);
        }
        let table = tokens.object_name()?;
        tokens.finish()?;
        return Ok(Some((table, index, if_exists)));
    }
    tokens.position = 0;
    if tokens.take_keyword("ALTER") && tokens.take_keyword("TABLE") {
        let table = tokens.object_name()?;
        if !tokens.take_keyword("DROP")
            || !(tokens.take_keyword("INDEX") || tokens.take_keyword("KEY"))
        {
            return Ok(None);
        }
        let if_exists = tokens.if_exists()?;
        let index = Ident::with_quote('`', tokens.identifier()?);
        tokens.finish()?;
        return Ok(Some((table, index, if_exists)));
    }
    Ok(None)
}

fn normalize_rename(sql: &str) -> Result<Option<String>> {
    if !may_start_with(sql, &["RENAME"]) {
        return Ok(None);
    }
    let mut tokens = Tokens::new(sql)?;
    if !tokens.take_keyword("RENAME") {
        return Ok(None);
    }
    tokens.keyword("TABLE")?;
    let from = tokens.object_name()?;
    tokens.keyword("TO")?;
    let to = tokens.object_name()?;
    tokens.finish()?;
    Ok(Some(format!("ALTER TABLE {from} RENAME TO {to}")))
}

pub(crate) fn parse_use(sql: &str) -> Result<Option<String>> {
    if !may_start_with(sql, &["USE"]) {
        return Ok(None);
    }
    let mut tokens = Tokens::new(sql)?;
    if !tokens.take_keyword("USE") {
        return Ok(None);
    }
    let database = tokens.database()?;
    tokens.finish()?;
    Ok(Some(database))
}

// MySqlDialect treats @ as an identifier start, including @ immediately before
// a quoted host. Account grammar needs an actual @ punctuation token instead.
#[derive(Debug)]
struct AccountDialect;
impl Dialect for AccountDialect {
    fn is_identifier_start(&self, character: char) -> bool {
        character != '@' && MySqlDialect {}.is_identifier_start(character)
    }
    fn is_identifier_part(&self, character: char) -> bool {
        character != '@' && MySqlDialect {}.is_identifier_part(character)
    }
    fn is_delimited_identifier_start(&self, character: char) -> bool {
        MySqlDialect {}.is_delimited_identifier_start(character)
    }
    fn supports_string_literal_backslash_escape(&self) -> bool {
        true
    }
}

struct Tokens {
    tokens: Vec<Token>,
    position: usize,
}
impl Tokens {
    fn new(sql: &str) -> Result<Self> {
        Ok(Self {
            tokens: Tokenizer::new(&AccountDialect, sql)
                .tokenize()?
                .into_iter()
                .filter(|token| !matches!(token, Token::Whitespace(_)))
                .collect(),
            position: 0,
        })
    }
    fn peek_keyword(&self, keyword: &str) -> bool {
        matches!(self.tokens.get(self.position), Some(Token::Word(word)) if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(keyword))
    }
    fn take_keyword(&mut self, keyword: &str) -> bool {
        if self.peek_keyword(keyword) {
            self.position += 1;
            true
        } else {
            false
        }
    }
    fn keyword(&mut self, keyword: &str) -> Result<()> {
        ensure!(self.take_keyword(keyword), "Expected {keyword}");
        Ok(())
    }
    fn take(&mut self, token: &Token) -> bool {
        if self.tokens.get(self.position) == Some(token) {
            self.position += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, token: &Token) -> Result<()> {
        ensure!(self.take(token), "Expected {token}");
        Ok(())
    }
    fn identifier(&mut self) -> Result<String> {
        match self.tokens.get(self.position) {
            Some(Token::Word(word)) => {
                let value = word.value.clone();
                self.position += 1;
                Ok(value)
            }
            _ => bail!("Expected identifier"),
        }
    }
    fn object_name(&mut self) -> Result<ObjectName> {
        let mut names = vec![Ident::with_quote('`', self.identifier()?)];
        while self.take(&Token::Period) {
            names.push(Ident::with_quote('`', self.identifier()?));
        }
        Ok(ObjectName(names))
    }
    fn string(&mut self) -> Result<String> {
        match self.tokens.get(self.position) {
            Some(Token::SingleQuotedString(value)) | Some(Token::DoubleQuotedString(value)) => {
                let value = value.clone();
                self.position += 1;
                Ok(value)
            }
            _ => bail!("Expected quoted string"),
        }
    }
    fn database(&mut self) -> Result<String> {
        let name = self.identifier()?;
        ensure!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
            "Database names must contain 1–64 ASCII letters, digits or underscores"
        );
        ensure!(
            !matches!(
                name.to_ascii_lowercase().as_str(),
                "mysql" | "sys" | "performance_schema" | "information_schema"
            ),
            "Reserved database name"
        );
        Ok(name)
    }
    fn account(&mut self) -> Result<String> {
        let username = if matches!(self.tokens.get(self.position), Some(Token::Word(_))) {
            self.identifier()?
        } else {
            self.string()?
        };
        ensure!(
            !username.is_empty() && username.len() <= 128 && !username.contains('\0'),
            "Invalid user name"
        );
        self.expect(&Token::AtSign)?;
        ensure!(
            self.string()? == "%",
            "Only '%' account hosts are supported by the embedded server"
        );
        Ok(username)
    }
    fn if_not_exists(&mut self) -> Result<bool> {
        if self.take_keyword("IF") {
            self.keyword("NOT")?;
            self.keyword("EXISTS")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn if_exists(&mut self) -> Result<bool> {
        if self.take_keyword("IF") {
            self.keyword("EXISTS")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn finish(&mut self) -> Result<()> {
        self.take(&Token::SemiColon);
        ensure!(
            self.position == self.tokens.len(),
            "Unsupported trailing SQL or multiple statements"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalization_tracks_table_and_column_qualifier_rewrites() {
        let catalog = Catalog::default();
        let admin = catalog.identity("root").unwrap();
        assert_eq!(
            catalog
                .authorize_and_normalize(&admin, "app", "SELECT app.items.id FROM items")
                .unwrap(),
            "SELECT items.id FROM items"
        );
        assert_eq!(
            catalog
                .authorize_and_normalize(&admin, "app", "SELECT id FROM app.items")
                .unwrap(),
            "SELECT id FROM items"
        );
        let original = "INSERT INTO items VALUES (1, 'unchanged text')";
        assert_eq!(
            catalog
                .authorize_and_normalize(&admin, "app", original)
                .unwrap(),
            original
        );
    }

    #[test]
    fn recognizer_dispatch_keeps_comments_and_rejects_trailing_commands() {
        assert_eq!(
            parse_use("/* leading */ uSe app").unwrap().as_deref(),
            Some("app")
        );
        assert!(
            AdminCommand::parse("/* leading */ CREATE DATABASE app")
                .unwrap()
                .is_some()
        );
        assert!(
            PreparedCommand::parse("-- comment\nPREPARE p FROM 'SELECT 1'")
                .unwrap()
                .is_some()
        );
        assert!(
            parse_index_drop("/* leading */ DROP INDEX idx ON items")
                .unwrap()
                .is_some()
        );
        assert!(
            parse_auto_increment_option("/* leading */ ALTER TABLE items AUTO_INCREMENT=2")
                .unwrap()
                .is_some()
        );
        assert!(
            normalize_rename("/* leading */ RENAME TABLE items TO renamed")
                .unwrap()
                .is_some()
        );
        assert!(parse_use("USE app; DROP DATABASE app").is_err());
        assert!(AdminCommand::parse("CREATE DATABASE app; DROP DATABASE app").is_err());
        assert!(PreparedCommand::parse("PREPARE p FROM 'SELECT 1'; DROP DATABASE app").is_err());
    }

    #[test]
    #[ignore = "manual comparison of command recognizer tokenization"]
    fn benchmark_command_recognizers() {
        let values = (0..200)
            .map(|id| format!("({id},'{}')", "payload".repeat(100)))
            .collect::<Vec<_>>()
            .join(",");
        let plain = format!("INSERT INTO items VALUES {values}");
        let fallback = format!("/**/ {plain}");
        for (label, sql) in [
            ("full-tokenizer", &fallback),
            ("keyword-dispatch", &plain),
            ("full-tokenizer", &fallback),
            ("keyword-dispatch", &plain),
        ] {
            let started = std::time::Instant::now();
            for _ in 0..50 {
                assert!(parse_use(sql).unwrap().is_none());
                assert!(PreparedCommand::parse(sql).unwrap().is_none());
                assert!(AdminCommand::parse(sql).unwrap().is_none());
                assert!(parse_index_drop(sql).unwrap().is_none());
                assert!(parse_auto_increment_option(sql).unwrap().is_none());
                assert!(normalize_rename(sql).unwrap().is_none());
            }
            eprintln!(
                "{label}: 50 batches {:.3} ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    fn execute(catalog: &mut Catalog, identity: &Identity, sql: &str) -> Result<CatalogEffect> {
        catalog.apply(
            identity,
            AdminCommand::parse(sql)?.expect("administrative statement"),
        )
    }
    #[test]
    fn provisioning_and_revocation_enforce_database_boundary() -> Result<()> {
        let mut catalog = Catalog::default();
        let admin = catalog.identity("root")?;
        execute(
            &mut catalog,
            &admin,
            "CREATE DATABASE `shard_a` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
        )?;
        execute(
            &mut catalog,
            &admin,
            "CREATE USER IF NOT EXISTS 'tenant'@'%' IDENTIFIED BY 'secret'",
        )?;
        execute(
            &mut catalog,
            &admin,
            "REVOKE ALL PRIVILEGES, GRANT OPTION FROM 'tenant'@'%'",
        )?;
        execute(
            &mut catalog,
            &admin,
            "GRANT SELECT,INSERT,UPDATE,DELETE ON `shard_a`.* TO 'tenant'@'%'",
        )?;
        let tenant = catalog.identity("tenant")?;
        assert!(catalog.check_database(&tenant, "app").is_err());
        assert!(
            catalog
                .authorize_and_normalize(
                    &tenant,
                    "shard_a",
                    "SELECT * FROM shard_a.items WHERE id IN (SELECT id FROM app.items)"
                )
                .is_err()
        );
        assert_eq!(
            catalog.authorize_and_normalize(&tenant, "shard_a", "SELECT * FROM shard_a.items")?,
            "SELECT * FROM items"
        );
        assert!(
            catalog
                .authorize_and_normalize(
                    &tenant,
                    "shard_a",
                    "DELETE FROM items WHERE id IN (SELECT id FROM app.items)"
                )
                .is_err()
        );
        assert!(
            catalog
                .authorize_and_normalize(&tenant, "shard_a", "CREATE TABLE items (id INT)")
                .is_err()
        );
        assert!(
            catalog
                .authorize_and_normalize(&tenant, "shard_a", "SELECT 1; DELETE FROM items")
                .is_err()
        );
        assert!(execute(&mut catalog, &tenant, "CREATE DATABASE stolen").is_err());
        execute(
            &mut catalog,
            &admin,
            "REVOKE ALL PRIVILEGES, GRANT OPTION FROM 'tenant'@'%'",
        )?;
        assert!(
            catalog
                .authorize_and_normalize(&tenant, "shard_a", "SELECT * FROM items")
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn native_password_verification_and_fail_closed_grammar() -> Result<()> {
        let mut catalog = Catalog::default();
        catalog.set_admin_credentials("owner", "secret");
        let salt = b"01234567890123456789";
        let first = sha1_smol::Sha1::from("secret").digest().bytes();
        let second = sha1_smol::Sha1::from(&first).digest().bytes();
        let mut sha = sha1_smol::Sha1::new();
        sha.update(salt);
        sha.update(&second);
        let mask = sha.digest().bytes();
        let response: [u8; 20] = std::array::from_fn(|index| first[index] ^ mask[index]);
        assert!(catalog.authenticate("owner", salt, &response));
        assert!(!catalog.authenticate("owner", b"wrong salt", &response));
        assert!(!catalog.authenticate("owner", salt, &[]));
        assert!(!catalog.authenticate("root", salt, &[]));
        assert!(AdminCommand::parse("GRANT ALL ON app.* TO 'user'@'%'").is_err());
        assert!(
            AdminCommand::parse("GRANT SELECT ON app.* TO 'user'@'%' WITH GRANT OPTION").is_err()
        );
        assert!(
            AdminCommand::parse("CREATE USER 'user'@'localhost' IDENTIFIED BY 'secret'").is_err()
        );
        assert!(AdminCommand::parse("CREATE DATABASE good; DROP DATABASE app").is_err());
        assert_eq!(parse_use("USE `app`;")?, Some("app".into()));
        Ok(())
    }
}

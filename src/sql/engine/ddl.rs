use super::*;

impl RawEngine {
    pub(super) fn replace_table_from_result(&self, table: &str, result: QueryResult) -> Result<()> {
        self.schemas.remove(table);
        self.rows.remove(table);
        self.indexes.remove(table);
        self.clear_auto_inc(table);
        self.delete_table_from_storage(table)?;

        let mut schema = TableSchemaHint {
            table: table.to_string(),
            ..TableSchemaHint::default()
        };
        for column in &result.columns {
            let value = result.rows.first().and_then(|row| row.get(column));
            schema.column_order.push(column.clone());
            schema.columns.insert(
                column.clone(),
                ColumnHint {
                    sql_type: Some(inferred_sql_type(value)),
                    nullable: Some(value.is_none_or(Value::is_null)),
                    ..ColumnHint::default()
                },
            );
        }
        schema.updated_at = Some(Utc::now());
        self.schemas.insert(table.to_string(), schema);
        let mut table_rows = BTreeMap::new();
        for (index, row) in result.rows.into_iter().enumerate() {
            let key = (index + 1).to_string();
            let stored = StoredRow::new(
                table.to_string(),
                Value::Number(Number::from((index + 1) as u64)),
                row,
            );
            self.persist_row(table, &key, &stored)?;
            table_rows.insert(key, stored);
        }
        self.rows.insert(table.to_string(), table_rows);
        self.rebuild_indexes(table);
        self.persist_schema(table)?;
        Ok(())
    }

    pub(super) fn create_table_as_select(
        &self,
        name: ObjectName,
        columns: Vec<sqlparser::ast::ColumnDef>,
        constraints: Vec<TableConstraint>,
        if_not_exists: bool,
        temporary: bool,
        query: sqlparser::ast::Query,
    ) -> Result<QueryResult> {
        let result = self.select_query(query)?;
        let table = object_name(&name)?;
        if self.mysql_strict() && self.schemas.contains_key(&table) {
            if if_not_exists {
                return Ok(QueryResult::default());
            }
            return Err(anyhow!("table '{table}' already exists"));
        }
        let mut schema = table_schema_from_create(&table, columns, constraints);
        for (index, column) in result.columns.iter().enumerate() {
            let value = result.rows.first().and_then(|row| row.get(column));
            schema.column_order.push(column.clone());
            schema.columns.insert(
                column.clone(),
                ColumnHint {
                    sql_type: Some(inferred_sql_type(value)),
                    nullable: Some(value.is_none_or(Value::is_null)),
                    ..ColumnHint::default()
                },
            );
            let _ = index;
        }
        schema.temporary = temporary;
        schema.updated_at = Some(Utc::now());
        self.schemas.insert(table.clone(), schema);
        let mut table_rows = BTreeMap::new();
        for (index, row) in result.rows.into_iter().enumerate() {
            let id = Value::Number(Number::from((index + 1) as u64));
            let key = (index + 1).to_string();
            let stored = StoredRow::new(table.clone(), id, row);
            self.persist_row(&table, &key, &stored)?;
            table_rows.insert(key, stored);
        }
        self.rows.insert(table.clone(), table_rows);
        self.rebuild_indexes(&table);
        self.persist_schema(&table)?;
        Ok(QueryResult::default())
    }

    pub(super) fn create_table(
        &self,
        name: ObjectName,
        columns: Vec<sqlparser::ast::ColumnDef>,
        constraints: Vec<TableConstraint>,
        if_not_exists: bool,
        temporary: bool,
    ) -> Result<QueryResult> {
        let table = object_name(&name)?;
        if self.mysql_strict() && self.schemas.contains_key(&table) {
            if if_not_exists {
                return Ok(QueryResult::default());
            }
            return Err(anyhow!("table '{table}' already exists"));
        }
        if self.mysql_strict() {
            let mut seen = BTreeSet::new();
            for column in &columns {
                if !seen.insert(column.name.value.to_ascii_lowercase()) {
                    return Err(anyhow!("duplicate column name: {}", column.name.value));
                }
            }
        }
        let mut incoming = table_schema_from_create(&table, columns, constraints);
        incoming.temporary = temporary;
        if self.mysql_strict() {
            self.validate_mysql_schema(&incoming)?;
        }
        let schema = if let Some(existing) = self.schemas.get(&table).map(|schema| schema.clone()) {
            let mut existing = existing;
            merge_create_table_schema(&mut existing, incoming);
            existing
        } else {
            incoming
        };

        self.schemas.insert(table.clone(), schema);
        self.rows.entry(table.clone()).or_default();
        self.rebuild_indexes(&table);
        self.persist_schema(&table)?;
        Ok(QueryResult::default())
    }

    pub(super) fn alter_table(
        &self,
        name: ObjectName,
        operations: Vec<sqlparser::ast::AlterTableOperation>,
        if_exists: bool,
    ) -> Result<QueryResult> {
        let table = object_name(&name)?;
        let existed = self.schemas.contains_key(&table);
        if self.mysql_strict() && !existed {
            if if_exists {
                return Ok(QueryResult::default());
            }
            return Err(anyhow!("unknown table: {table}"));
        }
        if let [sqlparser::ast::AlterTableOperation::RenameTable { table_name }] =
            operations.as_slice()
        {
            return self.rename_table(&table, &object_name(table_name)?);
        }
        let mut schema = self
            .schemas
            .get(&table)
            .map(|s| s.clone())
            .unwrap_or_else(|| TableSchemaHint {
                table: table.clone(),
                ..TableSchemaHint::default()
            });
        let mut rows_affected = 0;

        for op in operations {
            let rebuilds_rows = alter_operation_rebuilds_rows(&op);
            let row_action = alter_row_action(&op);
            self.apply_alter_operation(&table, &mut schema, op)?;
            if let Some(row_action) = row_action {
                self.apply_alter_row_action(&table, row_action)?;
            }
            if rebuilds_rows {
                rows_affected = self
                    .rows
                    .get(&table)
                    .map(|rows| rows.len() as u64)
                    .unwrap_or(0);
            }
        }

        if !existed && !schema_has_metadata(&schema) {
            return Ok(QueryResult::default());
        }

        schema.updated_at = Some(Utc::now());
        if self.mysql_strict() {
            self.validate_mysql_schema(&schema)?;
            if let Some(rows) = self.rows.get(&table).map(|rows| rows.clone()) {
                let previous_schema = self.schemas.get(&table).map(|schema| schema.clone());
                self.schemas.insert(table.clone(), schema.clone());
                let validation = self.validate_unique_constraints(&table, &rows);
                if let Some(previous_schema) = previous_schema {
                    self.schemas.insert(table.clone(), previous_schema);
                } else {
                    self.schemas.remove(&table);
                }
                validation?;
            }
        }
        self.schemas.insert(table.clone(), schema);
        self.rows.entry(table.clone()).or_default();
        self.rebuild_indexes(&table);
        self.persist_schema(&table)?;
        Ok(QueryResult {
            rows_affected,
            ..QueryResult::default()
        })
    }

    fn apply_alter_row_action(&self, table: &str, action: AlterRowAction) -> Result<()> {
        let Some(mut rows) = self.rows.get(table).map(|rows| rows.clone()) else {
            return Ok(());
        };
        for row in rows.values_mut() {
            match &action {
                AlterRowAction::Rename { old, new } => {
                    if let Some(value) = row.data.remove(old) {
                        row.data.insert(new.clone(), value);
                    }
                }
            }
        }
        for (key, row) in &rows {
            self.persist_row(table, key, row)?;
        }
        self.rows.insert(table.to_string(), rows);
        Ok(())
    }

    pub(super) fn apply_alter_operation(
        &self,
        table: &str,
        schema: &mut TableSchemaHint,
        op: sqlparser::ast::AlterTableOperation,
    ) -> Result<()> {
        match op {
            sqlparser::ast::AlterTableOperation::AddColumn {
                if_not_exists,
                column_def,
                column_position,
                ..
            } => {
                let column_name = column_def.name.value.clone();
                if self.mysql_strict() && schema.columns.contains_key(&column_name) {
                    if if_not_exists {
                        return Ok(());
                    }
                    return Err(anyhow!("duplicate column name: {column_name}"));
                }
                let column_hint = column_hint_from_def(&column_def);
                add_schema_column(schema, column_name.clone(), column_hint.clone());
                apply_column_position(schema, &column_name, column_position.as_ref())?;
                if column_hint.primary_key {
                    add_primary_key_metadata(schema, vec![column_name]);
                }
            }
            sqlparser::ast::AlterTableOperation::DropColumn {
                column_name,
                if_exists,
                ..
            } => {
                if !schema.columns.contains_key(&column_name.value) {
                    if if_exists || !self.mysql_strict() {
                        return Ok(());
                    }
                    return Err(anyhow!("unknown column: {}", column_name.value));
                }
                remove_column_metadata(schema, &column_name.value);
            }
            sqlparser::ast::AlterTableOperation::RenameColumn {
                old_column_name,
                new_column_name,
            } => {
                if self.mysql_strict() && !schema.columns.contains_key(&old_column_name.value) {
                    return Err(anyhow!("unknown column: {}", old_column_name.value));
                }
                if self.mysql_strict() && schema.columns.contains_key(&new_column_name.value) {
                    return Err(anyhow!("duplicate column name: {}", new_column_name.value));
                }
                rename_column_metadata(schema, &old_column_name.value, &new_column_name.value);
            }
            sqlparser::ast::AlterTableOperation::ChangeColumn {
                old_name,
                new_name,
                data_type,
                options,
                column_position,
            } => {
                if self.mysql_strict() && !schema.columns.contains_key(&old_name.value) {
                    return Err(anyhow!("unknown column: {}", old_name.value));
                }
                if self.mysql_strict()
                    && !old_name.value.eq_ignore_ascii_case(&new_name.value)
                    && schema.columns.contains_key(&new_name.value)
                {
                    return Err(anyhow!("duplicate column name: {}", new_name.value));
                }
                rename_column_metadata(schema, &old_name.value, &new_name.value);
                let hint = schema.columns.entry(new_name.value.clone()).or_default();
                hint.sql_type = Some(data_type.to_string());
                apply_column_options(hint, &options);
                apply_column_position(schema, &new_name.value, column_position.as_ref())?;
            }
            sqlparser::ast::AlterTableOperation::ModifyColumn {
                col_name,
                data_type,
                options,
                column_position,
            } => {
                if self.mysql_strict() && !schema.columns.contains_key(&col_name.value) {
                    return Err(anyhow!("unknown column: {}", col_name.value));
                }
                let hint = schema.columns.entry(col_name.value.clone()).or_default();
                hint.sql_type = Some(data_type.to_string());
                apply_column_options(hint, &options);
                apply_column_position(schema, &col_name.value, column_position.as_ref())?;
            }
            sqlparser::ast::AlterTableOperation::AlterColumn { column_name, op } => {
                let Some(hint) = schema.columns.get_mut(&column_name.value) else {
                    if self.mysql_strict() {
                        return Err(anyhow!("unknown column: {}", column_name.value));
                    }
                    return Ok(());
                };
                match op {
                    sqlparser::ast::AlterColumnOperation::SetNotNull => hint.nullable = Some(false),
                    sqlparser::ast::AlterColumnOperation::DropNotNull => hint.nullable = Some(true),
                    sqlparser::ast::AlterColumnOperation::SetDefault { value } => {
                        hint.default = Some(value.to_string())
                    }
                    sqlparser::ast::AlterColumnOperation::DropDefault => hint.default = None,
                    sqlparser::ast::AlterColumnOperation::SetDataType { data_type, .. } => {
                        hint.sql_type = Some(data_type.to_string())
                    }
                    _ => return Err(anyhow!("unsupported ALTER COLUMN operation")),
                }
            }
            other => {
                let applied = apply_alter_operation_fallback(table, schema, other)?;
                if self.mysql_strict() && !applied {
                    return Err(anyhow!("unsupported ALTER TABLE operation"));
                }
            }
        }

        Ok(())
    }

    pub(super) fn drop_table(
        &self,
        names: Vec<ObjectName>,
        if_exists: bool,
    ) -> Result<QueryResult> {
        if self.mysql_strict() {
            let dropping = names
                .iter()
                .map(object_name)
                .collect::<Result<BTreeSet<_>>>()?;
            for schema in self.schemas.iter() {
                if dropping.contains(&schema.table) {
                    continue;
                }
                if let Some(foreign_key) = schema
                    .foreign_keys
                    .iter()
                    .find(|foreign_key| dropping.contains(&foreign_key.referenced_table))
                {
                    return Err(anyhow!(
                        "cannot drop table referenced by foreign key constraint: {}",
                        foreign_key.name
                    ));
                }
            }
        }
        let mut warnings = Vec::new();
        for name in names {
            let table = object_name(&name)?;
            if self.views.contains_key(&table) {
                warnings.push(QueryWarning {
                    level: "Note".to_string(),
                    code: 1965,
                    message: format!("'test.{table}' is a view"),
                });
                continue;
            }
            if self.mysql_strict() && !self.schemas.contains_key(&table) {
                if if_exists {
                    continue;
                }
                return Err(anyhow!("unknown table: {table}"));
            }
            self.schemas.remove(&table);
            self.rows.remove(&table);
            self.indexes.remove(&table);
            self.clear_auto_inc(&table);
            self.delete_table_from_storage(&table)?;
        }
        Ok(QueryResult {
            warnings,
            ..QueryResult::default()
        })
    }

    pub(super) fn create_index_from_sql(&self, sql: &str) -> Result<QueryResult> {
        let Some(index) = parse_create_index_hint(sql)? else {
            return Err(anyhow!("unsupported CREATE INDEX syntax: {sql}"));
        };

        if index.or_replace && index.if_not_exists {
            return Err(anyhow!("Incorrect usage of OR REPLACE and IF NOT EXISTS"));
        }

        if self.mysql_strict() && !self.schemas.contains_key(&index.table) {
            return Err(anyhow!("unknown table: {}", index.table));
        }
        if self.mysql_strict() {
            let schema = self
                .schemas
                .get(&index.table)
                .map(|schema| schema.clone())
                .ok_or_else(|| anyhow!("unknown table: {}", index.table))?;
            if schema
                .indexes
                .iter()
                .any(|existing| existing.name.eq_ignore_ascii_case(&index.name))
            {
                if index.if_not_exists && !index.or_replace {
                    return Ok(QueryResult::default());
                }
                if !index.or_replace {
                    return Err(anyhow!("duplicate key name: {}", index.name));
                }
            }
            for column in &index.columns {
                if !schema.columns.contains_key(column) {
                    return Err(anyhow!("unknown column: {column}"));
                }
            }
        }

        let mut schema = self
            .schemas
            .get(&index.table)
            .map(|schema| schema.clone())
            .unwrap_or_else(|| TableSchemaHint {
                table: index.table.clone(),
                ..TableSchemaHint::default()
            });
        if index.or_replace {
            schema
                .indexes
                .retain(|existing| !existing.name.eq_ignore_ascii_case(&index.name));
        }
        add_index_metadata(
            &mut schema,
            IndexHint {
                name: index.name,
                columns: index.columns.clone(),
                unique: index.unique,
                prefix_lengths: index.prefix_lengths,
            },
        );
        if index.unique {
            add_unique_metadata(&mut schema, index.columns);
        }
        schema.updated_at = Some(Utc::now());
        self.schemas.insert(schema.table.clone(), schema.clone());
        self.rows.entry(schema.table.clone()).or_default();
        self.rebuild_indexes(&schema.table);
        self.persist_schema(&schema.table)?;
        Ok(QueryResult::default())
    }

    fn validate_mysql_schema(&self, schema: &TableSchemaHint) -> Result<()> {
        for column in schema
            .primary_key
            .iter()
            .chain(schema.unique.iter().flatten())
            .chain(schema.indexes.iter().flat_map(|index| index.columns.iter()))
        {
            if !schema
                .columns
                .keys()
                .any(|known| known.eq_ignore_ascii_case(column))
            {
                return Err(anyhow!("unknown column: {column}"));
            }
        }
        for foreign_key in &schema.foreign_keys {
            if foreign_key.columns.len() != foreign_key.referenced_columns.len()
                || foreign_key.columns.is_empty()
            {
                return Err(anyhow!(
                    "foreign key column count does not match: {}",
                    foreign_key.name
                ));
            }
            for column in &foreign_key.columns {
                if !schema
                    .columns
                    .keys()
                    .any(|known| known.eq_ignore_ascii_case(column))
                {
                    return Err(anyhow!("unknown column: {column}"));
                }
            }
            let Some(parent) = (if foreign_key
                .referenced_table
                .eq_ignore_ascii_case(&schema.table)
            {
                Some(schema.clone())
            } else {
                self.schemas
                    .get(&foreign_key.referenced_table)
                    .map(|schema| schema.clone())
            }) else {
                // MariaDB accepts forward references while the referenced
                // table is not yet present; enforce column checks once the
                // parent schema exists.
                continue;
            };
            for column in &foreign_key.referenced_columns {
                if !parent
                    .columns
                    .keys()
                    .any(|known| known.eq_ignore_ascii_case(column))
                {
                    return Err(anyhow!("unknown column: {column}"));
                }
            }
        }
        Ok(())
    }

    pub(super) fn drop_index(
        &self,
        names: Vec<ObjectName>,
        if_exists: bool,
    ) -> Result<QueryResult> {
        let index_names = names
            .into_iter()
            .map(|name| object_name(&name))
            .collect::<Result<Vec<_>>>()?;
        let tables = self
            .schemas
            .iter()
            .map(|schema| schema.key().clone())
            .collect::<Vec<_>>();
        let mut result = QueryResult::default();
        for table in tables {
            for index_name in &index_names {
                let existed = self.schemas.get(&table).is_some_and(|schema| {
                    schema
                        .indexes
                        .iter()
                        .any(|index| index.name.eq_ignore_ascii_case(index_name))
                        || schema.unique.iter().any(|unique| {
                            unique
                                .first()
                                .is_some_and(|name| name.eq_ignore_ascii_case(index_name))
                        })
                });
                self.drop_index_from_table(&table, index_name)?;
                if if_exists && !existed {
                    result.warnings.push(QueryWarning {
                        level: "Note".to_string(),
                        code: 1091,
                        message: format!("Can't DROP INDEX `{index_name}`; check that it exists"),
                    });
                }
            }
        }
        Ok(result)
    }

    pub(super) fn drop_index_from_table(
        &self,
        table: &str,
        index_name: &str,
    ) -> Result<QueryResult> {
        let Some(mut schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            if self.mysql_strict() {
                return Err(anyhow!("unknown table: {table}"));
            }
            return Ok(QueryResult::default());
        };
        let before = schema.indexes.len() + schema.unique.len();
        drop_unique_metadata(&mut schema, index_name);
        if schema.indexes.len() + schema.unique.len() != before {
            schema.updated_at = Some(Utc::now());
            self.schemas.insert(table.to_string(), schema);
            self.index_comments.remove(&format!("{table}:{index_name}"));
            self.rebuild_indexes(table);
            self.persist_schema(table)?;
        }
        Ok(QueryResult::default())
    }

    pub(super) fn truncate_tables(
        &self,
        table_names: Vec<sqlparser::ast::TruncateTableTarget>,
    ) -> Result<QueryResult> {
        for table_name in table_names {
            let table = object_name(&table_name.name)?;
            if !self.schemas.contains_key(&table) {
                if self.mysql_strict() {
                    return Err(anyhow!("unknown table: {table}"));
                }
                continue;
            }
            if let Some(rows) = self.rows.get(&table) {
                for _ in rows.values() {
                    record_query_row_write(0);
                }
            }
            self.rows.insert(table.clone(), BTreeMap::new());
            self.indexes.remove(&table);
            self.clear_auto_inc(&table);
            self.rebuild_indexes(&table);
            self.delete_table_rows_from_storage(&table)?;
        }
        self.persist_auto_inc()?;
        Ok(QueryResult::default())
    }
}

fn inferred_sql_type(value: Option<&Value>) -> String {
    match value {
        Some(Value::Bool(_)) => "BOOLEAN".to_string(),
        Some(Value::Number(number)) if number.is_i64() || number.is_u64() => "BIGINT".to_string(),
        Some(Value::Number(_)) => "DOUBLE".to_string(),
        Some(Value::Null) | None => "TEXT".to_string(),
        _ => "VARCHAR(255)".to_string(),
    }
}

fn alter_operation_rebuilds_rows(op: &sqlparser::ast::AlterTableOperation) -> bool {
    let operation = op.to_string();
    let operation = operation.trim_start().to_ascii_uppercase();
    operation.starts_with("MODIFY COLUMN ") || operation.starts_with("CHANGE COLUMN ")
}

enum AlterRowAction {
    Rename { old: String, new: String },
}

fn alter_row_action(op: &sqlparser::ast::AlterTableOperation) -> Option<AlterRowAction> {
    let tokens = normalized_sql_tokens(&op.to_string());
    if tokens.first()?.eq_ignore_ascii_case("RENAME")
        && tokens.get(1)?.eq_ignore_ascii_case("COLUMN")
    {
        return Some(AlterRowAction::Rename {
            old: tokens.get(2)?.clone(),
            new: tokens.get(4)?.clone(),
        });
    }
    if tokens.first()?.eq_ignore_ascii_case("CHANGE")
        && tokens.get(1)?.eq_ignore_ascii_case("COLUMN")
    {
        return Some(AlterRowAction::Rename {
            old: tokens.get(2)?.clone(),
            new: tokens.get(3)?.clone(),
        });
    }
    if tokens.first()?.eq_ignore_ascii_case("CHANGE") {
        return Some(AlterRowAction::Rename {
            old: tokens.get(1)?.clone(),
            new: tokens.get(2)?.clone(),
        });
    }
    None
}

pub(super) fn object_name(name: &ObjectName) -> Result<String> {
    if name.0.is_empty() {
        return Err(anyhow!("invalid object name"));
    }
    Ok(name
        .0
        .iter()
        .map(|identifier| identifier.value.clone())
        .collect::<Vec<_>>()
        .join("."))
}

pub(super) fn column_hint_from_def(col: &sqlparser::ast::ColumnDef) -> ColumnHint {
    let mut hint = ColumnHint {
        sql_type: Some(col.data_type.to_string()),
        ..ColumnHint::default()
    };

    for opt in &col.options {
        let text = opt.option.to_string().to_uppercase();
        if text.contains("NOT NULL") {
            hint.nullable = Some(false);
        }
        if text == "NULL" {
            hint.nullable = Some(true);
        }
        if text.contains("PRIMARY KEY") {
            hint.primary_key = true;
        }
        if text.contains("AUTO_INCREMENT") || text.contains("AUTOINCREMENT") {
            hint.auto_increment = true;
        }
        if let sqlparser::ast::ColumnOption::Default(expr) = &opt.option {
            hint.default = Some(expr.to_string());
        }
        if let sqlparser::ast::ColumnOption::Generated {
            generation_expr,
            generation_expr_mode,
            ..
        } = &opt.option
        {
            hint.generated = generation_expr.as_ref().map(ToString::to_string);
            hint.generated_stored = matches!(
                generation_expr_mode,
                Some(sqlparser::ast::GeneratedExpressionMode::Stored)
            );
        }
    }

    hint
}

fn apply_column_options(hint: &mut ColumnHint, options: &[sqlparser::ast::ColumnOption]) {
    for option in options {
        match option {
            sqlparser::ast::ColumnOption::Null => hint.nullable = Some(true),
            sqlparser::ast::ColumnOption::NotNull => hint.nullable = Some(false),
            sqlparser::ast::ColumnOption::Default(value) => hint.default = Some(value.to_string()),
            sqlparser::ast::ColumnOption::Unique { is_primary, .. } => {
                hint.primary_key = *is_primary;
                if *is_primary {
                    hint.nullable = Some(false);
                }
            }
            sqlparser::ast::ColumnOption::Generated {
                generation_expr,
                generation_expr_mode,
                ..
            } => {
                hint.generated = generation_expr.as_ref().map(ToString::to_string);
                hint.generated_stored = matches!(
                    generation_expr_mode,
                    Some(sqlparser::ast::GeneratedExpressionMode::Stored)
                );
            }
            sqlparser::ast::ColumnOption::DialectSpecific(tokens) => {
                let text = tokens
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_ascii_uppercase();
                if text.contains("AUTO_INCREMENT") || text.contains("AUTOINCREMENT") {
                    hint.auto_increment = true;
                }
            }
            _ => {}
        }
    }
}

fn apply_column_position(
    schema: &mut TableSchemaHint,
    column: &str,
    position: Option<&sqlparser::ast::MySQLColumnPosition>,
) -> Result<()> {
    let Some(position) = position else {
        return Ok(());
    };
    schema.column_order.retain(|known| known != column);
    match position {
        sqlparser::ast::MySQLColumnPosition::First => {
            schema.column_order.insert(0, column.to_string());
        }
        sqlparser::ast::MySQLColumnPosition::After(after) => {
            let index = schema
                .column_order
                .iter()
                .position(|known| known.eq_ignore_ascii_case(&after.value))
                .ok_or_else(|| anyhow!("unknown column: {}", after.value))?;
            schema.column_order.insert(index + 1, column.to_string());
        }
    }
    repair_column_order(schema);
    Ok(())
}

pub(super) fn table_schema_from_create(
    table: &str,
    columns: Vec<sqlparser::ast::ColumnDef>,
    constraints: Vec<TableConstraint>,
) -> TableSchemaHint {
    let mut hint = TableSchemaHint {
        table: table.to_string(),
        ..TableSchemaHint::default()
    };

    for col in columns {
        let mut column_hint = ColumnHint {
            sql_type: Some(col.data_type.to_string()),
            ..ColumnHint::default()
        };

        for opt in col.options {
            let text = opt.option.to_string().to_uppercase();
            if text.contains("NOT NULL") {
                column_hint.nullable = Some(false);
            }
            if text == "NULL" {
                column_hint.nullable = Some(true);
            }
            if text.contains("PRIMARY KEY") {
                column_hint.primary_key = true;
            }
            if text.contains("UNIQUE") {
                add_unique_metadata(&mut hint, vec![col.name.value.clone()]);
            }
            if text.contains("AUTO_INCREMENT") || text.contains("AUTOINCREMENT") {
                column_hint.auto_increment = true;
            }
            if let sqlparser::ast::ColumnOption::Default(expr) = &opt.option {
                column_hint.default = Some(expr.to_string());
            } else if let sqlparser::ast::ColumnOption::Generated {
                generation_expr,
                generation_expr_mode,
                ..
            } = &opt.option
            {
                column_hint.generated = generation_expr.as_ref().map(ToString::to_string);
                column_hint.generated_stored = matches!(
                    generation_expr_mode,
                    Some(sqlparser::ast::GeneratedExpressionMode::Stored)
                );
            } else if text.contains("REFERENCES") {
                let fk_text = format!("FOREIGN KEY ({}) {}", col.name.value, text);
                if let Some(foreign_key) = parse_foreign_key_hint(&hint.table, &fk_text) {
                    add_foreign_key_metadata(&mut hint, foreign_key);
                }
            }
        }

        if column_hint.primary_key {
            hint.primary_key.push(col.name.value.clone());
        }
        add_schema_column(&mut hint, col.name.value, column_hint);
    }

    for constraint in constraints {
        let constraint_text = constraint.to_string();
        match constraint {
            TableConstraint::Unique { name, columns, .. } => {
                add_unique_metadata_with_name(
                    &mut hint,
                    name.map(|name| name.value),
                    columns.into_iter().map(|c| c.value).collect::<Vec<_>>(),
                );
            }
            TableConstraint::PrimaryKey { columns, .. } => {
                hint.primary_key = columns.into_iter().map(|c| c.value).collect();
            }
            TableConstraint::Index { name, columns, .. } => {
                hint.indexes.push(IndexHint {
                    name: name
                        .map(|name| name.value)
                        .unwrap_or_else(|| format!("{}_idx", hint.table)),
                    columns: columns.into_iter().map(|column| column.value).collect(),
                    unique: false,
                    prefix_lengths: Vec::new(),
                });
            }
            _ => {}
        }
        if let Some(foreign_key) = parse_foreign_key_hint(&hint.table, &constraint_text) {
            if !foreign_key.columns.is_empty()
                && !hint.primary_key.starts_with(&foreign_key.columns)
                && !hint
                    .unique
                    .iter()
                    .any(|columns| columns.starts_with(&foreign_key.columns))
                && !hint
                    .indexes
                    .iter()
                    .any(|index| index.columns.starts_with(&foreign_key.columns))
            {
                add_index_metadata(
                    &mut hint,
                    IndexHint {
                        name: foreign_key.columns[0].clone(),
                        columns: foreign_key.columns.clone(),
                        unique: false,
                        prefix_lengths: vec![],
                    },
                );
            }
            add_foreign_key_metadata(&mut hint, foreign_key);
        }
    }

    if !hint.primary_key.is_empty() {
        let primary_key = hint.primary_key.clone();
        add_primary_key_metadata(&mut hint, primary_key);
    }
    hint.updated_at = Some(Utc::now());
    hint
}

pub(super) fn add_schema_column(schema: &mut TableSchemaHint, column: String, hint: ColumnHint) {
    if !schema.columns.contains_key(&column) {
        schema.column_order.push(column.clone());
    }
    schema.columns.insert(column, hint);
    repair_column_order(schema);
}

pub(super) fn repair_column_order(schema: &mut TableSchemaHint) {
    let mut seen = BTreeSet::new();
    schema
        .column_order
        .retain(|column| schema.columns.contains_key(column) && seen.insert(column.clone()));
    for column in schema.columns.keys() {
        if !seen.contains(column) {
            schema.column_order.push(column.clone());
        }
    }
}

pub(super) fn ordered_schema_columns(schema: &TableSchemaHint) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for column in &schema.column_order {
        if schema.columns.contains_key(column) && seen.insert(column.clone()) {
            out.push(column.clone());
        }
    }
    for column in schema.columns.keys() {
        if seen.insert(column.clone()) {
            out.push(column.clone());
        }
    }
    out
}

pub(super) fn seed_row_columns(rows: &[Map<String, Value>]) -> Vec<String> {
    let mut columns = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        for column in row.keys() {
            if seen.insert(column.clone()) {
                columns.push(column.clone());
            }
        }
    }
    columns
}

pub(super) fn generated_position_column(position: usize) -> String {
    format!("column_{position}")
}

pub(super) fn merge_create_table_schema(existing: &mut TableSchemaHint, incoming: TableSchemaHint) {
    for column in ordered_schema_columns(&incoming) {
        if let Some(hint) = incoming.columns.get(&column).cloned() {
            add_schema_column(existing, column, hint);
        }
    }
    for (column, hint) in incoming.columns {
        if !existing.columns.contains_key(&column) {
            add_schema_column(existing, column, hint);
        }
    }
    if existing.primary_key.is_empty() && !incoming.primary_key.is_empty() {
        existing.primary_key = incoming.primary_key;
    }
    for unique in incoming.unique {
        add_unique_metadata(existing, unique);
    }
    for index in incoming.indexes {
        add_index_metadata(existing, index);
    }
    for foreign_key in incoming.foreign_keys {
        add_foreign_key_metadata(existing, foreign_key);
    }
    existing.updated_at = Some(Utc::now());
}

pub(super) fn schema_has_metadata(schema: &TableSchemaHint) -> bool {
    !schema.columns.is_empty()
        || !schema.primary_key.is_empty()
        || !schema.unique.is_empty()
        || !schema.indexes.is_empty()
        || !schema.foreign_keys.is_empty()
}

pub(super) fn apply_alter_operation_fallback(
    table: &str,
    schema: &mut TableSchemaHint,
    op: sqlparser::ast::AlterTableOperation,
) -> Result<bool> {
    let text = op.to_string();
    let upper = text.to_uppercase();
    let tokens = normalized_sql_tokens(&text);

    let applied = if upper.starts_with("DROP COLUMN ") {
        if let Some(col) = tokens.get(2) {
            remove_column_metadata(schema, col);
        }
        true
    } else if upper.starts_with("RENAME COLUMN ") {
        if let (Some(old), Some(new)) = (tokens.get(2), tokens.get(4)) {
            rename_column_metadata(schema, old, new);
        }
        true
    } else if upper.starts_with("CHANGE COLUMN ") {
        if let (Some(old), Some(new)) = (tokens.get(2), tokens.get(3)) {
            rename_column_metadata(schema, old, new);
            update_column_type_from_tokens(schema, new, &tokens, 4);
        }
        true
    } else if upper.starts_with("MODIFY COLUMN ") {
        if let Some(col) = tokens.get(2) {
            update_column_type_from_tokens(schema, col, &tokens, 3);
        }
        true
    } else if upper.contains("FOREIGN KEY")
        && (upper.starts_with("ADD ") || upper.starts_with("ADD CONSTRAINT"))
    {
        if let Some(foreign_key) = parse_foreign_key_hint(&schema.table, &text) {
            add_foreign_key_metadata(schema, foreign_key);
        }
        true
    } else if upper.starts_with("ADD PRIMARY KEY")
        || (upper.starts_with("ADD CONSTRAINT") && upper.contains("PRIMARY KEY"))
    {
        if let Some(cols) = columns_inside_parentheses(&text) {
            add_primary_key_metadata(schema, cols);
        }
        true
    } else if upper.starts_with("ADD UNIQUE") || upper.starts_with("ADD CONSTRAINT") {
        if let Some((cols, prefix_lengths)) = parse_index_columns(&text) {
            add_unique_metadata_with_name(
                schema,
                unique_name_from_alter_operation(&text),
                cols.clone(),
            );
            if let Some(index) = schema
                .indexes
                .iter_mut()
                .find(|index| index.unique && index.columns == cols)
            {
                index.prefix_lengths = prefix_lengths;
            }
        }
        true
    } else if upper.starts_with("ADD INDEX ") || upper.starts_with("ADD KEY ") {
        if let Some((cols, prefix_lengths)) = parse_index_columns(&text) {
            let name = tokens
                .get(2)
                .cloned()
                .unwrap_or_else(|| format!("{}_{}_idx", schema.table, cols.join("_")));
            add_index_metadata(
                schema,
                IndexHint {
                    name,
                    columns: cols,
                    unique: false,
                    prefix_lengths,
                },
            );
        }
        true
    } else if upper.starts_with("DROP INDEX ") || upper.starts_with("DROP KEY ") {
        if let Some(index) = tokens.get(2) {
            drop_unique_metadata(schema, index);
        }
        true
    } else if upper.starts_with("DROP PRIMARY KEY") {
        drop_primary_key_metadata(schema);
        true
    } else if upper.starts_with("DROP CONSTRAINT ") {
        if let Some(name) = tokens.get(2) {
            drop_unique_metadata(schema, name);
            drop_foreign_key_metadata(schema, name);
        }
        true
    } else if upper.starts_with("DROP FOREIGN KEY ") {
        if let Some(name) = tokens.get(3) {
            drop_foreign_key_metadata(schema, name);
        }
        true
    } else {
        tracing::debug!(
            table,
            operation = %text,
            "ignored unsupported ALTER TABLE metadata operation"
        );
        false
    };

    Ok(applied)
}

pub(super) fn normalized_sql_tokens(sql: &str) -> Vec<String> {
    sql.replace([',', '(', ')'], " ")
        .split_whitespace()
        .map(|token| token.trim_matches('`').to_string())
        .collect()
}

pub(super) fn remove_column_metadata(schema: &mut TableSchemaHint, col: &str) {
    schema.columns.remove(col);
    schema.column_order.retain(|column| column != col);
    schema.primary_key.retain(|pk| pk != col);
    for unique in &mut schema.unique {
        unique.retain(|u| u != col);
    }
    schema.unique.retain(|unique| !unique.is_empty());
    for index in &mut schema.indexes {
        index.columns.retain(|indexed| indexed != col);
    }
    schema.indexes.retain(|index| !index.columns.is_empty());
    for foreign_key in &mut schema.foreign_keys {
        foreign_key.columns.retain(|fk_col| fk_col != col);
    }
    schema
        .foreign_keys
        .retain(|foreign_key| !foreign_key.columns.is_empty());
}

pub(super) fn rename_column_metadata(schema: &mut TableSchemaHint, old: &str, new: &str) {
    if let Some(hint) = schema.columns.remove(old) {
        schema.columns.insert(new.to_string(), hint);
    }
    for column in &mut schema.column_order {
        if column == old {
            *column = new.to_string();
        }
    }
    for pk in &mut schema.primary_key {
        if pk == old {
            *pk = new.to_string();
        }
    }
    for unique in &mut schema.unique {
        for col in unique {
            if col == old {
                *col = new.to_string();
            }
        }
    }
    for index in &mut schema.indexes {
        for col in &mut index.columns {
            if col == old {
                *col = new.to_string();
            }
        }
    }
    for foreign_key in &mut schema.foreign_keys {
        for col in &mut foreign_key.columns {
            if col == old {
                *col = new.to_string();
            }
        }
    }
}

pub(super) fn update_column_type_from_tokens(
    schema: &mut TableSchemaHint,
    col: &str,
    tokens: &[String],
    type_idx: usize,
) {
    if !schema.columns.contains_key(col) {
        add_schema_column(schema, col.to_string(), ColumnHint::default());
    }
    let hint = schema.columns.entry(col.to_string()).or_default();
    if let Some(sql_type) = tokens.get(type_idx) {
        hint.sql_type = Some(
            tokens
                .get(type_idx + 1)
                .filter(|next| next.chars().all(|c| c.is_ascii_digit()))
                .map(|next| format!("{sql_type}({next})"))
                .unwrap_or_else(|| sql_type.clone()),
        );
    }
    let upper = tokens
        .iter()
        .map(|token| token.to_uppercase())
        .collect::<Vec<_>>();
    if upper.windows(2).any(|w| w == ["NOT", "NULL"]) {
        hint.nullable = Some(false);
    }
    if upper.iter().any(|token| token == "AUTO_INCREMENT") {
        hint.auto_increment = true;
    }
}

pub(super) fn columns_inside_parentheses(sql: &str) -> Option<Vec<String>> {
    let start = sql.find('(')?;
    let end = sql[start + 1..].find(')')? + start + 1;
    let cols = sql[start + 1..end]
        .split(',')
        .map(|col| col.trim().trim_matches('`').to_string())
        .filter(|col| !col.is_empty())
        .collect::<Vec<_>>();
    (!cols.is_empty()).then_some(cols)
}

fn parse_index_columns(sql: &str) -> Option<(Vec<String>, Vec<Option<u32>>)> {
    let start = sql.find('(')?;
    let mut depth = 0_i32;
    let mut end = None;
    for (offset, ch) in sql[start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(start + offset);
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &sql[start + 1..end?];
    let mut specs = Vec::new();
    let mut current = String::new();
    let mut nested = 0_i32;
    for ch in body.chars().chain(std::iter::once(',')) {
        match ch {
            '(' => {
                nested += 1;
                current.push(ch);
            }
            ')' => {
                nested -= 1;
                current.push(ch);
            }
            ',' if nested == 0 => {
                if !current.trim().is_empty() {
                    specs.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    let mut columns = Vec::new();
    let mut prefix_lengths = Vec::new();
    for mut spec in specs {
        for suffix in [" ASC", " DESC"] {
            if spec.to_ascii_uppercase().ends_with(suffix) {
                spec.truncate(spec.len() - suffix.len());
                spec = spec.trim().to_string();
            }
        }
        let (column, prefix) = if spec.ends_with(')') {
            if let Some(open) = spec.rfind('(') {
                let length = spec[open + 1..spec.len() - 1].trim().parse::<u32>().ok();
                if length.is_some() {
                    (&spec[..open], length)
                } else {
                    (spec.as_str(), None)
                }
            } else {
                (spec.as_str(), None)
            }
        } else {
            (spec.as_str(), None)
        };
        let column = column.trim().trim_matches('`').to_string();
        if column.is_empty() || column.contains(['(', ')']) {
            return None;
        }
        columns.push(column);
        prefix_lengths.push(prefix);
    }
    (!columns.is_empty()).then_some((columns, prefix_lengths))
}

struct ParsedIndexHint {
    name: String,
    table: String,
    columns: Vec<String>,
    unique: bool,
    prefix_lengths: Vec<Option<u32>>,
    if_not_exists: bool,
    or_replace: bool,
}

fn parse_create_index_hint(sql: &str) -> Result<Option<ParsedIndexHint>> {
    let tokens = normalized_sql_tokens(sql);
    let upper = tokens
        .iter()
        .map(|token| token.to_ascii_uppercase())
        .collect::<Vec<_>>();
    let Some(index_pos) = upper
        .iter()
        .position(|token| token == "INDEX" || token == "KEY")
    else {
        return Ok(None);
    };
    let Some(on_pos) = upper.iter().position(|token| token == "ON") else {
        return Ok(None);
    };
    let Some(name) = tokens.get(index_pos + 1).cloned() else {
        return Ok(None);
    };
    let (name, if_not_exists) = if name.eq_ignore_ascii_case("IF")
        && tokens
            .get(index_pos + 2)
            .is_some_and(|token| token.eq_ignore_ascii_case("NOT"))
        && tokens
            .get(index_pos + 3)
            .is_some_and(|token| token.eq_ignore_ascii_case("EXISTS"))
    {
        let Some(name) = tokens.get(index_pos + 4).cloned() else {
            return Ok(None);
        };
        (name, true)
    } else {
        (name, false)
    };
    let Some(table) = tokens.get(on_pos + 1).cloned() else {
        return Ok(None);
    };
    let (columns, prefix_lengths) = parse_index_columns(sql).unwrap_or_default();
    if columns.is_empty() {
        return Ok(None);
    }

    Ok(Some(ParsedIndexHint {
        name,
        table,
        columns,
        unique: upper.iter().any(|token| token == "UNIQUE"),
        prefix_lengths,
        if_not_exists,
        or_replace: upper.windows(2).any(|tokens| tokens == ["OR", "REPLACE"]),
    }))
}

pub(super) fn add_unique_metadata(schema: &mut TableSchemaHint, cols: Vec<String>) {
    add_unique_metadata_with_name(schema, None, cols);
}

pub(super) fn add_unique_metadata_with_name(
    schema: &mut TableSchemaHint,
    name: Option<String>,
    cols: Vec<String>,
) {
    if !schema.unique.iter().any(|existing| existing == &cols) {
        schema.unique.push(cols.clone());
    }
    if !cols.is_empty() {
        add_index_metadata(
            schema,
            IndexHint {
                name: name.unwrap_or_else(|| generated_index_name(&schema.table, &cols)),
                columns: cols,
                unique: true,
                prefix_lengths: vec![],
            },
        );
    }
}

fn unique_name_from_alter_operation(sql: &str) -> Option<String> {
    let prefix = sql.split_once('(').map(|(prefix, _)| prefix).unwrap_or(sql);
    let tokens = normalized_sql_tokens(prefix);
    if tokens.len() >= 3
        && tokens[0].eq_ignore_ascii_case("ADD")
        && tokens[1].eq_ignore_ascii_case("CONSTRAINT")
    {
        return tokens.get(2).cloned();
    }
    let unique_position = tokens
        .iter()
        .position(|token| token.eq_ignore_ascii_case("UNIQUE"))?;
    let mut name_position = unique_position + 1;
    if tokens.get(name_position).is_some_and(|token| {
        token.eq_ignore_ascii_case("INDEX") || token.eq_ignore_ascii_case("KEY")
    }) {
        name_position += 1;
    }
    tokens.get(name_position).cloned()
}

pub(super) fn add_primary_key_metadata(schema: &mut TableSchemaHint, cols: Vec<String>) {
    for col in cols {
        if !schema.primary_key.iter().any(|existing| existing == &col) {
            schema.primary_key.push(col.clone());
        }
        if let Some(hint) = schema.columns.get_mut(&col) {
            hint.primary_key = true;
            hint.nullable = Some(false);
        }
    }
    schema.indexes.retain(|index| index.name != "PRIMARY");
    if !schema.primary_key.is_empty() {
        add_index_metadata(
            schema,
            IndexHint {
                name: "PRIMARY".to_string(),
                columns: schema.primary_key.clone(),
                unique: true,
                prefix_lengths: vec![],
            },
        );
    }
}

pub(super) fn drop_primary_key_metadata(schema: &mut TableSchemaHint) {
    schema.primary_key.clear();
    schema.indexes.retain(|index| index.name != "PRIMARY");
}

pub(super) fn drop_unique_metadata(schema: &mut TableSchemaHint, index_name: &str) {
    let mut removed_columns = Vec::new();
    schema.indexes.retain(|index| {
        let drizzle_name = format!("{}_{}_unique", schema.table, index.columns.join("_"));
        let hit = index.name == index_name
            || index.columns.join("_") == index_name
            || (index.unique && drizzle_name == index_name);
        if hit {
            removed_columns.push(index.columns.clone());
        }
        !hit
    });
    schema.unique.retain(|cols| {
        if schema
            .indexes
            .iter()
            .any(|index| index.unique && index.columns == *cols)
        {
            return true;
        }
        let generated = generated_index_name(&schema.table, cols);
        let legacy_generated = legacy_generated_index_name(&schema.table, cols);
        let drizzle_name = format!("{}_{}_unique", schema.table, cols.join("_"));
        generated != index_name
            && legacy_generated != index_name
            && drizzle_name != index_name
            && cols.join("_") != index_name
            && !removed_columns.iter().any(|removed| removed == cols)
    });
}

pub(super) fn add_index_metadata(schema: &mut TableSchemaHint, index: IndexHint) {
    if !schema
        .indexes
        .iter()
        .any(|existing| existing.name == index.name)
    {
        schema.indexes.push(index);
    }
}

pub(super) fn generated_index_name(_table: &str, cols: &[String]) -> String {
    cols.first()
        .cloned()
        .unwrap_or_else(|| "unique".to_string())
}

fn legacy_generated_index_name(table: &str, cols: &[String]) -> String {
    format!("{}_{}_uniq", table, cols.join("_"))
}

pub(super) fn unique_index_name(schema: &TableSchemaHint, cols: &[String]) -> String {
    let configured = schema
        .indexes
        .iter()
        .find(|index| index.unique && index.columns == cols)
        .map(|index| index.name.as_str());
    let parser_generated_constraint = format!("{}_{}_CONSTRAINT", schema.table, cols.join("_"));
    match configured {
        Some(name) if name == parser_generated_constraint => {
            format!("{}_{}_unique", schema.table, cols.join("_"))
        }
        Some(name) => name.to_string(),
        None => generated_index_name(&schema.table, cols),
    }
}

pub(super) fn add_foreign_key_metadata(schema: &mut TableSchemaHint, foreign_key: ForeignKeyHint) {
    if !schema.foreign_keys.iter().any(|existing| {
        existing.name == foreign_key.name || existing.columns == foreign_key.columns
    }) {
        schema.foreign_keys.push(foreign_key);
    }
}

pub(super) fn drop_foreign_key_metadata(schema: &mut TableSchemaHint, name: &str) {
    schema.foreign_keys.retain(|foreign_key| {
        foreign_key.name != name
            && foreign_key.columns.join("_") != name
            && format!("{}_{}_fk", schema.table, foreign_key.columns.join("_")) != name
    });
}

pub(super) fn parse_foreign_key_hint(table: &str, sql: &str) -> Option<ForeignKeyHint> {
    let upper = sql.to_ascii_uppercase();
    let fk_pos = upper.find("FOREIGN KEY")?;
    let references_pos = upper.find("REFERENCES")?;
    let before_fk = sql[..fk_pos].trim();
    let name = normalized_sql_tokens(before_fk)
        .windows(2)
        .find_map(|window| {
            (window[0].eq_ignore_ascii_case("CONSTRAINT")).then(|| window[1].clone())
        })
        .unwrap_or_default();
    let columns = columns_inside_parentheses(&sql[fk_pos..])?;
    let references = &sql[references_pos + "REFERENCES".len()..];
    let ref_tokens = normalized_sql_tokens(references);
    let referenced_table = ref_tokens.first()?.clone();
    let referenced_columns = columns_inside_parentheses(references)?;
    let name = if name.is_empty() {
        format!("{}_{}_fk", table, columns.join("_"))
    } else {
        name
    };
    Some(ForeignKeyHint {
        name,
        columns,
        referenced_table,
        referenced_columns,
        on_delete: parse_referential_action(sql, "ON DELETE"),
        on_update: parse_referential_action(sql, "ON UPDATE"),
    })
}

pub(super) fn parse_referential_action(sql: &str, marker: &str) -> Option<String> {
    let upper = sql.to_ascii_uppercase();
    let pos = upper.find(marker)?;
    let tail = sql[pos + marker.len()..].trim();
    let tokens = normalized_sql_tokens(tail);
    match tokens.as_slice() {
        [first, second, ..] if first.eq_ignore_ascii_case("SET") => {
            Some(format!("SET {}", second.to_ascii_uppercase()))
        }
        [first, second, ..] if first.eq_ignore_ascii_case("NO") => {
            Some(format!("NO {}", second.to_ascii_uppercase()))
        }
        [first, ..] => Some(first.to_ascii_uppercase()),
        _ => None,
    }
}

pub(super) fn render_create_table(schema: &TableSchemaHint) -> String {
    let mut parts = Vec::new();
    for column in ordered_schema_columns(schema) {
        let Some(hint) = schema.columns.get(&column) else {
            continue;
        };
        let mut line = format!(
            "  `{}` {}",
            column,
            hint.sql_type.clone().unwrap_or_else(|| "TEXT".to_string())
        );
        if hint.nullable == Some(false) {
            line.push_str(" NOT NULL");
        }
        if let Some(default) = &hint.default {
            line.push_str(" DEFAULT ");
            line.push_str(default);
        }
        if hint.auto_increment {
            line.push_str(" AUTO_INCREMENT");
        }
        if let Some(generated) = &hint.generated {
            line.push_str(" GENERATED ALWAYS AS (");
            line.push_str(generated);
            line.push(')');
            line.push_str(if hint.generated_stored {
                " STORED"
            } else {
                " VIRTUAL"
            });
        }
        parts.push(line);
    }
    if !schema.primary_key.is_empty() {
        parts.push(format!(
            "  PRIMARY KEY ({})",
            schema
                .primary_key
                .iter()
                .map(|column| format!("`{column}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for unique in &schema.unique {
        let index = schema
            .indexes
            .iter()
            .find(|index| index.unique && index.columns == *unique);
        parts.push(format!(
            "  UNIQUE KEY `{}` ({})",
            unique_index_name(schema, unique),
            render_index_columns(unique, index.map(|index| &index.prefix_lengths))
        ));
    }
    for index in &schema.indexes {
        if index.unique || index.name == "PRIMARY" {
            continue;
        }
        parts.push(format!(
            "  KEY `{}` ({})",
            index.name,
            render_index_columns(&index.columns, Some(&index.prefix_lengths))
        ));
    }
    for foreign_key in &schema.foreign_keys {
        let mut line = format!(
            "  CONSTRAINT `{}` FOREIGN KEY ({}) REFERENCES `{}` ({})",
            foreign_key.name,
            foreign_key
                .columns
                .iter()
                .map(|column| format!("`{column}`"))
                .collect::<Vec<_>>()
                .join(", "),
            foreign_key.referenced_table,
            foreign_key
                .referenced_columns
                .iter()
                .map(|column| format!("`{column}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let Some(action) = &foreign_key.on_delete {
            line.push_str(" ON DELETE ");
            line.push_str(action);
        }
        if let Some(action) = &foreign_key.on_update {
            line.push_str(" ON UPDATE ");
            line.push_str(action);
        }
        parts.push(line);
    }
    format!(
        "CREATE {}TABLE `{}` (\n{}\n)",
        if schema.temporary { "TEMPORARY " } else { "" },
        schema.table,
        parts.join(",\n")
    )
}

fn render_index_columns(columns: &[String], prefix_lengths: Option<&Vec<Option<u32>>>) -> String {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let prefix = prefix_lengths
                .and_then(|prefixes| prefixes.get(index))
                .copied()
                .flatten();
            prefix
                .map(|prefix| format!("`{column}`({prefix})"))
                .unwrap_or_else(|| format!("`{column}`"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

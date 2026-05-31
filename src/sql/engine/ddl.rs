use super::*;

impl Engine {
    pub(super) fn create_table(
        &self,
        name: ObjectName,
        columns: Vec<sqlparser::ast::ColumnDef>,
        constraints: Vec<TableConstraint>,
    ) -> Result<QueryResult> {
        let table = object_name(&name)?;
        let incoming = table_schema_from_create(&table, columns, constraints);
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
    ) -> Result<QueryResult> {
        let table = object_name(&name)?;
        let existed = self.schemas.contains_key(&table);
        let mut schema = self
            .schemas
            .get(&table)
            .map(|s| s.clone())
            .unwrap_or_else(|| TableSchemaHint {
                table: table.clone(),
                ..TableSchemaHint::default()
            });

        for op in operations {
            self.apply_alter_operation(&table, &mut schema, op)?;
        }

        if !existed && !schema_has_metadata(&schema) {
            return Ok(QueryResult::default());
        }

        schema.updated_at = Some(Utc::now());
        self.schemas.insert(table.clone(), schema);
        self.rows.entry(table.clone()).or_default();
        self.rebuild_indexes(&table);
        self.persist_schema(&table)?;
        Ok(QueryResult::default())
    }

    pub(super) fn apply_alter_operation(
        &self,
        table: &str,
        schema: &mut TableSchemaHint,
        op: sqlparser::ast::AlterTableOperation,
    ) -> Result<()> {
        match op {
            sqlparser::ast::AlterTableOperation::AddColumn { column_def, .. } => {
                add_schema_column(
                    schema,
                    column_def.name.value.clone(),
                    column_hint_from_def(&column_def),
                );
            }
            other => apply_alter_operation_fallback(table, schema, other)?,
        }

        Ok(())
    }

    pub(super) fn drop_table(&self, names: Vec<ObjectName>) -> Result<QueryResult> {
        for name in names {
            let table = object_name(&name)?;
            self.schemas.remove(&table);
            self.rows.remove(&table);
            self.indexes.remove(&table);
            self.clear_auto_inc(&table);
            self.delete_table_from_storage(&table)?;
        }
        Ok(QueryResult::default())
    }

    pub(super) fn create_index_from_sql(&self, sql: &str) -> Result<QueryResult> {
        let Some(index) = parse_create_index_hint(sql)? else {
            return Err(anyhow!("unsupported CREATE INDEX syntax: {sql}"));
        };

        let mut schema = self
            .schemas
            .get(&index.table)
            .map(|schema| schema.clone())
            .unwrap_or_else(|| TableSchemaHint {
                table: index.table.clone(),
                ..TableSchemaHint::default()
            });
        add_index_metadata(
            &mut schema,
            IndexHint {
                name: index.name,
                columns: index.columns.clone(),
                unique: index.unique,
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

    pub(super) fn drop_index(&self, names: Vec<ObjectName>) -> Result<QueryResult> {
        let index_names = names
            .into_iter()
            .map(|name| object_name(&name))
            .collect::<Result<Vec<_>>>()?;
        let tables = self
            .schemas
            .iter()
            .map(|schema| schema.key().clone())
            .collect::<Vec<_>>();
        for table in tables {
            let Some(mut schema) = self.schemas.get(&table).map(|schema| schema.clone()) else {
                continue;
            };
            let before = schema.indexes.len() + schema.unique.len();
            for index_name in &index_names {
                drop_unique_metadata(&mut schema, index_name);
            }
            if schema.indexes.len() + schema.unique.len() != before {
                schema.updated_at = Some(Utc::now());
                self.schemas.insert(table.clone(), schema);
                self.rebuild_indexes(&table);
                self.persist_schema(&table)?;
            }
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
                continue;
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

pub(super) fn object_name(name: &ObjectName) -> Result<String> {
    name.0
        .last()
        .map(|i| i.value.clone())
        .ok_or_else(|| anyhow!("invalid object name"))
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
    }

    hint
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
            if let sqlparser::ast::ColumnOption::Default(expr) = opt.option {
                column_hint.default = Some(expr.to_string());
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
            TableConstraint::Unique { columns, .. } => {
                add_unique_metadata(
                    &mut hint,
                    columns.into_iter().map(|c| c.value).collect::<Vec<_>>(),
                );
            }
            TableConstraint::PrimaryKey { columns, .. } => {
                hint.primary_key = columns.into_iter().map(|c| c.value).collect();
            }
            _ => {}
        }
        if let Some(foreign_key) = parse_foreign_key_hint(&hint.table, &constraint_text) {
            add_foreign_key_metadata(&mut hint, foreign_key);
        }
    }

    if !hint.primary_key.is_empty() {
        let primary_key = hint.primary_key.clone();
        add_index_metadata(
            &mut hint,
            IndexHint {
                name: "PRIMARY".to_string(),
                columns: primary_key,
                unique: true,
            },
        );
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
) -> Result<()> {
    let text = op.to_string();
    let upper = text.to_uppercase();
    let tokens = normalized_sql_tokens(&text);

    if upper.starts_with("DROP COLUMN ") {
        if let Some(col) = tokens.get(2) {
            remove_column_metadata(schema, col);
        }
    } else if upper.starts_with("RENAME COLUMN ") {
        if let (Some(old), Some(new)) = (tokens.get(2), tokens.get(4)) {
            rename_column_metadata(schema, old, new);
        }
    } else if upper.starts_with("CHANGE COLUMN ") {
        if let (Some(old), Some(new)) = (tokens.get(2), tokens.get(3)) {
            rename_column_metadata(schema, old, new);
            update_column_type_from_tokens(schema, new, &tokens, 4);
        }
    } else if upper.starts_with("MODIFY COLUMN ") {
        if let Some(col) = tokens.get(2) {
            update_column_type_from_tokens(schema, col, &tokens, 3);
        }
    } else if upper.contains("FOREIGN KEY")
        && (upper.starts_with("ADD ") || upper.starts_with("ADD CONSTRAINT"))
    {
        if let Some(foreign_key) = parse_foreign_key_hint(&schema.table, &text) {
            add_foreign_key_metadata(schema, foreign_key);
        }
    } else if upper.starts_with("ADD UNIQUE") || upper.starts_with("ADD CONSTRAINT") {
        if let Some(cols) = columns_inside_parentheses(&text) {
            add_unique_metadata(schema, cols);
        }
    } else if upper.starts_with("ADD INDEX ") || upper.starts_with("ADD KEY ") {
        if let Some(cols) = columns_inside_parentheses(&text) {
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
                },
            );
        }
    } else if upper.starts_with("DROP INDEX ") || upper.starts_with("DROP KEY ") {
        if let Some(index) = tokens.get(2) {
            drop_unique_metadata(schema, index);
        }
    } else if upper.starts_with("DROP CONSTRAINT ") {
        if let Some(name) = tokens.get(2) {
            drop_unique_metadata(schema, name);
            drop_foreign_key_metadata(schema, name);
        }
    } else if upper.starts_with("DROP FOREIGN KEY ") {
        if let Some(name) = tokens.get(3) {
            drop_foreign_key_metadata(schema, name);
        }
    } else {
        tracing::debug!(
            table,
            operation = %text,
            "ignored unsupported ALTER TABLE metadata operation"
        );
    }

    Ok(())
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

struct ParsedIndexHint {
    name: String,
    table: String,
    columns: Vec<String>,
    unique: bool,
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
    let Some(table) = tokens.get(on_pos + 1).cloned() else {
        return Ok(None);
    };
    let columns = columns_inside_parentheses(sql).unwrap_or_default();
    if columns.is_empty() {
        return Ok(None);
    }

    Ok(Some(ParsedIndexHint {
        name,
        table,
        columns,
        unique: upper.iter().any(|token| token == "UNIQUE"),
    }))
}

pub(super) fn add_unique_metadata(schema: &mut TableSchemaHint, cols: Vec<String>) {
    if !schema.unique.iter().any(|existing| existing == &cols) {
        schema.unique.push(cols.clone());
    }
    if !cols.is_empty() {
        add_index_metadata(
            schema,
            IndexHint {
                name: generated_index_name(&schema.table, &cols),
                columns: cols,
                unique: true,
            },
        );
    }
}

pub(super) fn drop_unique_metadata(schema: &mut TableSchemaHint, index_name: &str) {
    let mut removed_columns = Vec::new();
    schema.indexes.retain(|index| {
        let hit = index.name == index_name || index.columns.join("_") == index_name;
        if hit {
            removed_columns.push(index.columns.clone());
        }
        !hit
    });
    schema.unique.retain(|cols| {
        let generated = generated_index_name(&schema.table, cols);
        generated != index_name
            && cols.join("_") != index_name
            && !removed_columns.iter().any(|removed| removed == cols)
    });
}

pub(super) fn add_index_metadata(schema: &mut TableSchemaHint, index: IndexHint) {
    if !schema.indexes.iter().any(|existing| {
        existing.name == index.name
            || (existing.columns == index.columns && existing.unique == index.unique)
    }) {
        schema.indexes.push(index);
    }
}

pub(super) fn generated_index_name(table: &str, cols: &[String]) -> String {
    format!("{}_{}_uniq", table, cols.join("_"))
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
        parts.push(format!(
            "  UNIQUE KEY `{}` ({})",
            generated_index_name(&schema.table, unique),
            unique
                .iter()
                .map(|column| format!("`{column}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for index in &schema.indexes {
        if index.unique || index.name == "PRIMARY" {
            continue;
        }
        parts.push(format!(
            "  KEY `{}` ({})",
            index.name,
            index
                .columns
                .iter()
                .map(|column| format!("`{column}`"))
                .collect::<Vec<_>>()
                .join(", ")
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
        "CREATE TABLE `{}` (\n{}\n)",
        schema.table,
        parts.join(",\n")
    )
}

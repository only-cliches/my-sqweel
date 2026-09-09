use my_sqweel::sql::engine::Engine;

#[test]
fn information_schema_engines_lists_available_engines() {
    let engine = Engine::default();
    let result = engine
        .execute_sql(
            "SELECT engine, support FROM information_schema.engines WHERE engine = 'InnoDB'",
        )
        .unwrap();
    assert_eq!(result[0].rows.len(), 1);
    assert!(result[0].columns.contains(&"engine".to_string()));
    assert!(result[0].columns.contains(&"support".to_string()));
}

#[test]
fn information_schema_processlist_shows_current_connection() {
    let engine = Engine::default();
    let result = engine
        .execute_sql("SELECT id, user, host, db FROM information_schema.processlist")
        .unwrap();
    assert_eq!(
        result[0].rows.len(),
        1,
        "Should show at least one connection"
    );
    assert!(result[0].columns.contains(&"id".to_string()));
    assert!(result[0].columns.contains(&"user".to_string()));
}

#[test]
fn information_schema_session_variables_lists_variables() {
    let engine = Engine::default();
    let result = engine
        .execute_sql("SELECT variable_name FROM information_schema.session_variables WHERE variable_name = 'VERSION'")
        .unwrap();
    assert!(result[0].rows.len() > 0);
}

#[test]
fn information_schema_global_variables_same_as_session() {
    let engine = Engine::default();
    let session = engine
        .execute_sql("SELECT variable_name FROM information_schema.session_variables")
        .unwrap();
    let global = engine
        .execute_sql("SELECT variable_name FROM information_schema.global_variables")
        .unwrap();
    assert!(session[0].rows.len() > 0);
    assert!(global[0].rows.len() > 0);
    assert_eq!(
        session[0].rows.len(),
        global[0].rows.len(),
        "Session and global variables should be the same"
    );
}

#[test]
fn information_schema_keywords_contains_sql_reserved_words() {
    let engine = Engine::default();
    let result = engine
        .execute_sql("SELECT keyword FROM information_schema.keywords WHERE keyword = 'SELECT'")
        .unwrap();
    assert_eq!(result[0].rows.len(), 1, "SELECT should be in keywords");

    let result = engine
        .execute_sql("SELECT keyword FROM information_schema.keywords WHERE keyword = 'FROM'")
        .unwrap();
    assert_eq!(result[0].rows.len(), 1, "FROM should be in keywords");
}

#[test]
fn information_schema_keywords_has_common_keywords() {
    let engine = Engine::default();
    let result = engine
        .execute_sql("SELECT keyword FROM information_schema.keywords")
        .unwrap();
    assert!(result[0].rows.len() > 100, "Should have 100+ keywords");

    // Check for specific keywords
    assert!(result[0].rows.iter().any(|row| {
        row.get("keyword")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == "CREATE")
    }));
}

#[test]
fn information_schema_triggers_is_queryable() {
    let engine = Engine::default();
    let result = engine
        .execute_sql("SELECT * FROM information_schema.triggers")
        .unwrap();
    // Should be queryable (returns empty is fine)
    assert!(result.len() > 0);
}

#[test]
fn information_schema_check_constraints_is_queryable() {
    let engine = Engine::default();
    let result = engine
        .execute_sql("SELECT * FROM information_schema.check_constraints")
        .unwrap();
    // Should be queryable (returns empty is fine)
    assert!(result.len() > 0);
}

#[test]
fn empty_information_schema_views_retains_result_columns() {
    let engine = Engine::default();
    let result = engine
        .execute_sql("SELECT * FROM information_schema.views WHERE table_schema = 'app'")
        .unwrap();

    assert!(result[0].rows.is_empty());
    assert_eq!(
        result[0].columns,
        [
            "table_schema",
            "table_name",
            "view_definition",
            "check_option",
            "security_type"
        ]
    );
}

#[test]
fn empty_information_schema_columns_retains_drizzle_result_columns() {
    let engine = Engine::default();
    let result = engine
        .execute_sql(
            "select * from information_schema.columns \
             where table_schema = 'app' and table_name != '__drizzle_migrations' \
             order by table_name, ordinal_position;",
        )
        .unwrap();

    assert!(result[0].rows.is_empty());
    assert_eq!(
        result[0].columns,
        [
            "table_schema",
            "table_name",
            "column_name",
            "ordinal_position",
            "is_nullable",
            "column_default",
            "column_type",
            "data_type",
            "character_set_name",
            "collation_name",
            "generation_expression",
            "column_key",
            "extra",
        ]
    );
}

#[test]
fn empty_information_schema_statistics_retains_drizzle_result_columns() {
    let engine = Engine::default();
    let result = engine
        .execute_sql(
            "select * from INFORMATION_SCHEMA.STATISTICS \
             WHERE INFORMATION_SCHEMA.STATISTICS.TABLE_SCHEMA = 'app' \
             and INFORMATION_SCHEMA.STATISTICS.INDEX_NAME != 'PRIMARY';",
        )
        .unwrap();

    assert!(result[0].rows.is_empty());
    assert_eq!(
        result[0].columns,
        [
            "table_schema",
            "table_name",
            "index_name",
            "column_name",
            "seq_in_index",
            "non_unique",
        ]
    );
}

#[test]
fn empty_information_schema_wildcards_are_result_sets() {
    let engine = Engine::default();
    for table in [
        "tables",
        "table_constraints",
        "key_column_usage",
        "referential_constraints",
        "routines",
        "triggers",
        "check_constraints",
        "files",
    ] {
        let result = engine
            .execute_sql(&format!("SELECT * FROM information_schema.{table}"))
            .unwrap();
        assert!(
            !result[0].columns.is_empty(),
            "empty information_schema.{table} must retain its result columns"
        );
    }
}

#[test]
fn information_schema_files_is_queryable() {
    let engine = Engine::default();
    let result = engine
        .execute_sql("SELECT * FROM information_schema.files")
        .unwrap();
    // Should be queryable (returns empty is fine)
    assert!(result.len() > 0);
}

#[test]
fn information_schema_engines_has_standard_columns() {
    let engine = Engine::default();
    let result = engine
        .execute_sql(
            "SELECT engine, support, comment, transactions FROM information_schema.engines LIMIT 1",
        )
        .unwrap();
    assert!(result[0].rows.len() > 0);
    assert!(result[0].columns.contains(&"engine".to_string()));
    assert!(result[0].columns.contains(&"support".to_string()));
}

#[test]
fn information_schema_processlist_filtering_works() {
    let engine = Engine::default();
    let result = engine
        .execute_sql("SELECT * FROM information_schema.processlist WHERE id = 1")
        .unwrap();
    assert_eq!(result[0].rows.len(), 1);

    let result = engine
        .execute_sql("SELECT * FROM information_schema.processlist WHERE id = 999")
        .unwrap();
    assert_eq!(result[0].rows.len(), 0);
}

#[test]
fn information_schema_engines_supports_common_queries() {
    let engine = Engine::default();

    // Count how many engines
    let result = engine
        .execute_sql("SELECT engine FROM information_schema.engines")
        .unwrap();
    assert!(result[0].rows.len() > 0, "Should have at least one engine");

    // Filter by support
    let result = engine
        .execute_sql("SELECT engine FROM information_schema.engines WHERE support = 'YES'")
        .unwrap();
    assert!(
        result[0].rows.len() > 0,
        "Should have at least one supported engine"
    );
}

#[test]
fn information_schema_session_variables_supports_filtering() {
    let engine = Engine::default();

    // Filter by variable name pattern
    let result = engine
        .execute_sql("SELECT variable_value FROM information_schema.session_variables WHERE variable_name = 'AUTOCOMMIT'")
        .unwrap();
    assert!(result[0].rows.len() > 0);

    // Non-existent variable
    let result = engine
        .execute_sql("SELECT * FROM information_schema.session_variables WHERE variable_name = 'NONEXISTENT'")
        .unwrap();
    assert_eq!(result[0].rows.len(), 0);
}

#[test]
fn information_schema_constraint_join_exposes_primary_key_columns() {
    let engine = Engine::default();
    engine
        .execute_sql(
            "CREATE TABLE memberships (organization_id BIGINT, user_id BIGINT, PRIMARY KEY (organization_id, user_id))",
        )
        .unwrap();

    let result = engine
        .execute_sql(
            "SELECT table_name, column_name, ordinal_position
             FROM information_schema.table_constraints t
             LEFT JOIN information_schema.key_column_usage k
             USING(constraint_name, table_schema, table_name)
             WHERE t.constraint_type = 'PRIMARY KEY'
               AND t.table_schema = 'app'
             ORDER BY ordinal_position",
        )
        .unwrap();

    assert_eq!(result[0].rows.len(), 2);
    assert_eq!(
        result[0].rows[0]
            .get("column_name")
            .and_then(|value| value.as_str()),
        Some("organization_id")
    );
    assert_eq!(
        result[0].rows[1]
            .get("column_name")
            .and_then(|value| value.as_str()),
        Some("user_id")
    );
}

#[test]
fn information_schema_statistics_includes_unique_constraints() {
    let engine = Engine::default();
    engine
        .execute_sql(
            "CREATE TABLE users (id BIGINT PRIMARY KEY, email VARCHAR(255), CONSTRAINT users_email_unique UNIQUE (email))",
        )
        .unwrap();

    let result = engine
        .execute_sql(
            "SELECT index_name, column_name, non_unique
             FROM information_schema.statistics
             WHERE table_schema = 'app' AND table_name = 'users' AND index_name != 'PRIMARY'",
        )
        .unwrap();

    assert_eq!(result[0].rows.len(), 1);
    assert_eq!(
        result[0].rows[0]
            .get("index_name")
            .and_then(|value| value.as_str()),
        Some("users_email_unique")
    );
    assert_eq!(
        result[0].rows[0]
            .get("non_unique")
            .and_then(|value| value.as_u64()),
        Some(0)
    );
}

#[test]
fn explicit_unique_names_survive_keyword_rewriting() {
    for constraint in [
        "CONSTRAINT custom_unique UNIQUE (email)",
        "CONSTRAINT customunique UNIQUE (email)",
        "UNIQUE custom_unique (email)",
    ] {
        let engine = Engine::default();
        engine
            .execute_sql(&format!("CREATE TABLE users (email TEXT, {constraint})"))
            .unwrap();
        let result = engine
            .execute_sql(
                "SELECT index_name FROM information_schema.statistics WHERE table_name = 'users'",
            )
            .unwrap();
        let expected = if constraint.contains("customunique") {
            "customunique"
        } else {
            "custom_unique"
        };
        assert_eq!(result[0].rows.len(), 1, "{constraint}");
        assert_eq!(result[0].rows[0]["index_name"], expected, "{constraint}");
    }
}

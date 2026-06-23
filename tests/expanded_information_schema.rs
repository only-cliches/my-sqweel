use my_sqweel::sql::engine::Engine;

#[test]
fn information_schema_engines_lists_available_engines() {
    let engine = Engine::default();
    let result = engine
        .execute_sql("SELECT engine, support FROM information_schema.engines WHERE engine = 'InnoDB'")
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
    assert_eq!(result[0].rows.len(), 1, "Should show at least one connection");
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
    assert!(result[0]
        .rows
        .iter()
        .any(|row| row.get("keyword")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == "CREATE")));
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
    assert!(result[0].rows.len() > 0, "Should have at least one supported engine");
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

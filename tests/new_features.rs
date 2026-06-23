use my_sqweel::sql::engine::Engine;

#[test]
fn test_between_operator() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE t (id INT, score INT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    let result = engine
        .execute_sql("SELECT id FROM t WHERE score BETWEEN 15 AND 25")
        .unwrap();
    assert_eq!(result[0].rows.len(), 1, "BETWEEN should match score = 20");
}

#[test]
fn test_case_when_expression() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE t (id INT, score INT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    let result = engine
        .execute_sql(
            "SELECT CASE WHEN score > 25 THEN 'high' WHEN score > 15 THEN 'mid' ELSE 'low' END FROM t",
        )
        .unwrap();
    assert_eq!(result[0].rows.len(), 3, "CASE should return 3 results");
}

#[test]
fn test_substring_function() {
    let engine = Engine::default();
    let result = engine.execute_sql("SELECT SUBSTRING('hello world', 1, 5)").unwrap();
    assert_eq!(result[0].rows.len(), 1);
    let row = &result[0].rows[0];
    let col_name = &result[0].columns[0];
    let val = row.get(col_name);
    assert!(val.is_some(), "Should have substring result");
}

#[test]
fn test_floor_ceil_functions() {
    let engine = Engine::default();
    let result = engine.execute_sql("SELECT FLOOR(10.7), CEIL(10.3)").unwrap();
    assert_eq!(result[0].rows.len(), 1);
}

#[test]
fn test_replace_function() {
    let engine = Engine::default();
    let result = engine
        .execute_sql("SELECT REPLACE('hello world', 'world', 'there')")
        .unwrap();
    assert_eq!(result[0].rows.len(), 1);
}

#[test]
fn test_like_case_insensitive() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE t (email VARCHAR(255))")
        .unwrap();
    engine
        .execute_sql("INSERT INTO t VALUES ('Test@Example.com'), ('admin@EXAMPLE.COM')")
        .unwrap();
    let result = engine
        .execute_sql("SELECT email FROM t WHERE email LIKE '%example%'")
        .unwrap();
    assert_eq!(
        result[0].rows.len(),
        2,
        "LIKE should be case-insensitive and find both rows"
    );
}

#[test]
fn test_group_concat_aggregate() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE t (category TEXT, item TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO t VALUES ('A', 'apple'), ('A', 'apricot'), ('B', 'banana')")
        .unwrap();
    let result = engine
        .execute_sql("SELECT category, GROUP_CONCAT(item) FROM t GROUP BY category")
        .unwrap();
    assert_eq!(result[0].rows.len(), 2, "Should have 2 groups");
}

#[test]
fn test_information_schema_tables_has_table_type() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE mytable (id INT)")
        .unwrap();
    let result = engine
        .execute_sql("SELECT table_name, table_type FROM information_schema.tables WHERE table_name = 'mytable'")
        .unwrap();
    assert_eq!(result[0].rows.len(), 1);
    assert!(
        result[0].columns.contains(&"table_type".to_string()),
        "Should have table_type column"
    );
}

#[test]
fn test_insert_select() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE source (id INT, val TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO source VALUES (1, 'a'), (2, 'b')")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE dest (id INT, val TEXT)")
        .unwrap();
    let result = engine
        .execute_sql("INSERT INTO dest SELECT * FROM source")
        .unwrap();
    assert_eq!(result[0].rows_affected, 2, "Should insert 2 rows");

    let check = engine.execute_sql("SELECT COUNT(*) FROM dest").unwrap();
    assert_eq!(check[0].rows.len(), 1);
}

#[test]
fn test_union() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE t1 (id INT)")
        .unwrap();
    engine.execute_sql("INSERT INTO t1 VALUES (1), (2)").unwrap();
    engine
        .execute_sql("CREATE TABLE t2 (id INT)")
        .unwrap();
    engine.execute_sql("INSERT INTO t2 VALUES (2), (3)").unwrap();

    let result = engine
        .execute_sql("SELECT id FROM t1 UNION SELECT id FROM t2")
        .unwrap();
    assert_eq!(result[0].rows.len(), 3, "UNION should deduplicate and return 3 rows");
}

#[test]
fn test_union_all() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE t1 (id INT)")
        .unwrap();
    engine.execute_sql("INSERT INTO t1 VALUES (1), (2)").unwrap();
    engine
        .execute_sql("CREATE TABLE t2 (id INT)")
        .unwrap();
    engine.execute_sql("INSERT INTO t2 VALUES (2), (3)").unwrap();

    let result = engine
        .execute_sql("SELECT id FROM t1 UNION ALL SELECT id FROM t2")
        .unwrap();
    assert_eq!(
        result[0].rows.len(),
        4,
        "UNION ALL should not deduplicate and return 4 rows"
    );
}

#[test]
fn test_implicit_cross_join() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE t1 (id INT)")
        .unwrap();
    engine.execute_sql("INSERT INTO t1 VALUES (1), (2)").unwrap();
    engine
        .execute_sql("CREATE TABLE t2 (id INT)")
        .unwrap();
    engine.execute_sql("INSERT INTO t2 VALUES (3), (4)").unwrap();

    let result = engine
        .execute_sql("SELECT t1.id, t2.id FROM t1, t2 WHERE t1.id = 1")
        .unwrap();
    assert_eq!(
        result[0].rows.len(),
        2,
        "Cross join should produce 2 rows (1 x 2)"
    );
}

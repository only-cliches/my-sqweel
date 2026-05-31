import assert from "node:assert/strict";
import mysql from "mysql2/promise";

const url = process.argv[2] ?? "mysql://root@127.0.0.1:3307/app";
const connection = await mysql.createConnection({ uri: url });

try {
  await connection.query("DROP TABLE IF EXISTS node_mysql2_users");
  await connection.query(
    "CREATE TABLE node_mysql2_users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255) UNIQUE, name TEXT)"
  );
  await connection.query(
    "INSERT INTO node_mysql2_users (email, name) VALUES (?, ?), (?, ?)",
    ["a@example.com", "Alice", "b@example.com", "Bob"]
  );

  const [textRows] = await connection.query(
    "SELECT email, name FROM node_mysql2_users ORDER BY id"
  );
  assert.deepEqual(textRows, [
    { email: "a@example.com", name: "Alice" },
    { email: "b@example.com", name: "Bob" }
  ]);

  const [rows] = await connection.execute(
    "SELECT email, name FROM node_mysql2_users WHERE id = ?",
    [1]
  );
  assert.deepEqual(rows, [{ email: "a@example.com", name: "Alice" }]);

  await connection.query(
    "INSERT INTO node_mysql2_users (email, name) VALUES ('a@example.com', 'Updated') ON DUPLICATE KEY UPDATE name = VALUES(name)"
  );
  const [updated] = await connection.query(
    "SELECT name FROM node_mysql2_users WHERE email = 'a@example.com'"
  );
  assert.deepEqual(updated, [{ name: "Updated" }]);

  await connection.query("USE app");
  await connection.query("SET time_zone = '+00:00', autocommit = 1");
  const [sessionRows] = await connection.query(
    "SELECT DATABASE() AS db, @@autocommit AS autocommit, @@time_zone AS tz"
  );
  assert.deepEqual(sessionRows, [{ db: "app", autocommit: 1, tz: "+00:00" }]);
} finally {
  await connection.end();
}

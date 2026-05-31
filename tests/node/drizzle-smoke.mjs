import assert from "node:assert/strict";
import mysql from "mysql2/promise";
import { drizzle } from "drizzle-orm/mysql2";
import { eq } from "drizzle-orm";
import { bigint, mysqlTable, text, varchar } from "drizzle-orm/mysql-core";

const users = mysqlTable("node_drizzle_users", {
  id: bigint("id", { mode: "number" }).primaryKey().autoincrement(),
  email: varchar("email", { length: 255 }).notNull().unique(),
  name: text("name")
});

const url = process.argv[2] ?? "mysql://root@127.0.0.1:3307/app";
const connection = await mysql.createConnection({ uri: url });
const db = drizzle(connection);

try {
  await connection.query("DROP TABLE IF EXISTS node_drizzle_users");
  await connection.query(
    "CREATE TABLE node_drizzle_users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255) UNIQUE NOT NULL, name TEXT)"
  );

  await db.insert(users).values({ email: "a@example.com", name: "Alice" });
  const rows = await db.select().from(users).where(eq(users.email, "a@example.com"));

  assert.equal(rows.length, 1);
  assert.equal(rows[0].email, "a@example.com");
  assert.equal(rows[0].name, "Alice");

  await db.update(users).set({ name: "Ada" }).where(eq(users.email, "a@example.com"));
  const updated = await db.select().from(users).where(eq(users.id, 1));
  assert.equal(updated[0].name, "Ada");

  await db.delete(users).where(eq(users.email, "a@example.com"));
  const empty = await db.select().from(users);
  assert.equal(empty.length, 0);
} finally {
  await connection.end();
}

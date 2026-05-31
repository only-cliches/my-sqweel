import assert from "node:assert/strict";
import mysql from "mysql2/promise";
import { drizzle } from "drizzle-orm/mysql2";
import {
  and,
  asc,
  count,
  desc,
  eq,
  gt,
  gte,
  inArray,
  isNotNull,
  isNull,
  like,
  lt,
  ne,
  or,
  sum
} from "drizzle-orm";
import { bigint, boolean, mysqlTable, text, varchar } from "drizzle-orm/mysql-core";

const users = mysqlTable("node_drizzle_compat_users", {
  id: bigint("id", { mode: "number" }).primaryKey().autoincrement(),
  email: varchar("email", { length: 255 }).notNull().unique(),
  name: text("name").notNull(),
  nickname: text("nickname"),
  score: bigint("score", { mode: "number" }).notNull(),
  active: boolean("active").notNull(),
  createdAt: text("created_at").notNull()
});

const posts = mysqlTable("node_drizzle_compat_posts", {
  id: bigint("id", { mode: "number" }).primaryKey().autoincrement(),
  userId: bigint("user_id", { mode: "number" }).notNull(),
  title: text("title").notNull(),
  published: boolean("published").notNull(),
  views: bigint("views", { mode: "number" }).notNull()
});

const url = process.argv[2] ?? "mysql://root@127.0.0.1:3307/app";
const connection = await mysql.createConnection({ uri: url });
const db = drizzle(connection);

try {
  await connection.query("DROP TABLE IF EXISTS node_drizzle_compat_posts");
  await connection.query("DROP TABLE IF EXISTS node_drizzle_compat_users");
  await connection.query(
    "CREATE TABLE node_drizzle_compat_users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255) UNIQUE NOT NULL, name TEXT NOT NULL, nickname TEXT, score BIGINT NOT NULL, active BOOLEAN NOT NULL, created_at TEXT NOT NULL)"
  );
  await connection.query(
    "CREATE TABLE node_drizzle_compat_posts (id BIGINT PRIMARY KEY AUTO_INCREMENT, user_id BIGINT NOT NULL, title TEXT NOT NULL, published BOOLEAN NOT NULL, views BIGINT NOT NULL)"
  );

  await db.insert(users).values([
    {
      email: "a@example.com",
      name: "Alice",
      nickname: null,
      score: 10,
      active: true,
      createdAt: "2026-01-02 10:20:30"
    },
    {
      email: "b@example.com",
      name: "Bob",
      nickname: "bee",
      score: 20,
      active: true,
      createdAt: "2026-01-03 11:22:33"
    },
    {
      email: "c@example.com",
      name: "Cara",
      nickname: null,
      score: 30,
      active: false,
      createdAt: "2026-01-04 12:24:36"
    },
    {
      email: "d@example.net",
      name: "Dave",
      nickname: null,
      score: 40,
      active: true,
      createdAt: "2026-01-05 13:26:39"
    }
  ]);

  await db.insert(posts).values([
    { userId: 1, title: "a-first", published: true, views: 5 },
    { userId: 1, title: "a-second", published: true, views: 7 },
    { userId: 2, title: "b-draft", published: false, views: 11 },
    { userId: 3, title: "c-first", published: true, views: 13 }
  ]);

  const filtered = await db
    .select({ email: users.email })
    .from(users)
    .where(
      and(
        like(users.email, "%example.com"),
        or(isNull(users.nickname), gt(users.score, 15)),
        inArray(users.email, ["a@example.com", "b@example.com", "c@example.com"]),
        ne(users.name, "Nobody"),
        isNotNull(users.createdAt)
      )
    )
    .orderBy(desc(users.score), asc(users.id))
    .limit(2)
    .offset(0);
  assert.deepEqual(filtered.map((row) => row.email), [
    "c@example.com",
    "b@example.com"
  ]);

  const ranged = await db
    .select({ email: users.email })
    .from(users)
    .where(and(gte(users.score, 10), lt(users.score, 30)))
    .orderBy(asc(users.id));
  assert.deepEqual(ranged.map((row) => row.email), ["a@example.com", "b@example.com"]);

  const joined = await db
    .select({ email: users.email, title: posts.title })
    .from(users)
    .leftJoin(posts, eq(posts.userId, users.id))
    .where(eq(users.email, "a@example.com"))
    .orderBy(asc(posts.id));
  assert.deepEqual(joined, [
    { email: "a@example.com", title: "a-first" },
    { email: "a@example.com", title: "a-second" }
  ]);

  const leftJoinMiss = await db
    .select({ email: users.email, title: posts.title })
    .from(users)
    .leftJoin(posts, eq(posts.userId, users.id))
    .where(eq(users.email, "d@example.net"));
  assert.deepEqual(leftJoinMiss, [{ email: "d@example.net", title: null }]);

  const grouped = await db
    .select({ userId: posts.userId, postCount: count(posts.id), totalViews: sum(posts.views) })
    .from(posts)
    .where(eq(posts.published, true))
    .groupBy(posts.userId)
    .having(({ postCount }) => gt(postCount, 1))
    .orderBy(asc(posts.userId));
  assert.equal(grouped.length, 1);
  assert.equal(grouped[0].userId, 1);
  assert.equal(grouped[0].postCount, 2);
  assert.equal(Number(grouped[0].totalViews), 12);

  await db
    .insert(users)
    .values({
      email: "a@example.com",
      name: "Alicia",
      nickname: "ally",
      score: 99,
      active: true,
      createdAt: "2026-01-06 14:28:42"
    })
    .onDuplicateKeyUpdate({ set: { name: "Alicia", nickname: "ally", score: 99 } });

  const upserted = await db
    .select({
      email: users.email,
      name: users.name,
      nickname: users.nickname,
      score: users.score
    })
    .from(users)
    .where(eq(users.email, "a@example.com"));
  assert.deepEqual(upserted, [
    { email: "a@example.com", name: "Alicia", nickname: "ally", score: 99 }
  ]);

  await db.delete(users).where(or(eq(users.email, "d@example.net"), lt(users.score, 15)));
  const remaining = await db.select({ email: users.email }).from(users).orderBy(asc(users.id));
  assert.deepEqual(remaining.map((row) => row.email), [
    "a@example.com",
    "b@example.com",
    "c@example.com"
  ]);
} finally {
  await connection.query("DROP TABLE IF EXISTS node_drizzle_compat_posts");
  await connection.query("DROP TABLE IF EXISTS node_drizzle_compat_users");
  await connection.end();
}

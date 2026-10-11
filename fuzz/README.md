# Coverage guided fuzzing

Install `cargo-fuzz` once with `cargo install cargo-fuzz`, then run a target from the repository root:

If the native RocksDB build cannot load `libclang`, set `LIBCLANG_PATH` to the directory containing `libclang.so` before building.

```sh
cargo fuzz run parser -- -max_len=4096
cargo fuzz run binding -- -max_len=2048
cargo fuzz run expression_eval -- -max_len=2048
cargo fuzz run query_execution -- -max_len=4096
cargo fuzz run dml_transactions -- -max_len=4096
cargo fuzz run mysql_wire -- -max_len=1048576
```

Targets have small checked input bounds and checked-in starter corpora under `corpus/<target>/`. LibFuzzer saves crashes and interesting inputs there (or in its artifact directory); replay a saved input with the same command and its file path. Collect native coverage with `cargo fuzz coverage <target>`.

The SQL targets fuzz parsing, engine binding/type resolution, expression evaluation, query execution, and bounded DML/transaction sequences. The wire target exercises the vendored MySQL packet framing parsers directly. Results are crash/timeout findings; they do not compare behavior with MariaDB.

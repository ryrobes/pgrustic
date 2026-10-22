//! Catalog install after an empty `rvbbit` schema, the semantic op suite
//! inside `BEGIN READ ONLY`, grants on `rvbbit_semantic_user`, and
//! classify batching under `count(*)`.

use std::io::Write;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

fn pg_bin() -> String {
    std::env::var("PG_BIN").unwrap_or_else(|_| "/usr/lib/postgresql/18/bin".into())
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn psql(port: u16, sql: &str) -> String {
    let out = Command::new(format!("{}/psql", pg_bin()))
        .args(["-h", "/tmp", "-p", &port.to_string(), "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-tA", "-c", sql])
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .output()
        .expect("psql");
    if !out.status.success() {
        panic!(
            "psql failed: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
    }
    // A multi-statement -c still prints BEGIN/SET/COMMIT next to the row.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_command_tag(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_command_tag(line: &str) -> bool {
    matches!(line, "BEGIN" | "COMMIT" | "ROLLBACK" | "SET" | "RESET")
        || line.starts_with("CREATE ")
        || line.starts_with("GRANT ")
        || line.starts_with("INSERT ")
        || line.starts_with("ALTER ")
        || line.starts_with("DROP ")
}

fn psql_file(port: u16, path: &str) {
    let out = Command::new(format!("{}/psql", pg_bin()))
        .args(["-h", "/tmp", "-p", &port.to_string(), "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-f", path])
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .output()
        .expect("psql");
    if !out.status.success() {
        panic!(
            "psql file failed: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

#[test]
fn readonly_semantic_suite_grants_and_batches() {
    let pg_port = free_port();
    let hutch_port = free_port();
    assert_ne!(pg_port, 55432);
    assert_ne!(hutch_port, 8099);
    let listener = TcpListener::bind(("127.0.0.1", hutch_port)).unwrap();
    thread::spawn(move || {
        let cfg = hutch_mock::Config {
            latency: Duration::ZERO,
            fail_every: 0,
            timeout_every: 0,
            expect_auth: None,
        };
        let _ = hutch_mock::serve_listener(listener, cfg);
    });

    let data = format!("/tmp/rvbbit-ro-{pg_port}");
    let _ = std::fs::remove_dir_all(&data);
    let init = Command::new(format!("{}/initdb", pg_bin()))
        .args(["-D", &data, "--no-locale", "--encoding=UTF8", "--auth-local=trust", "-U", "postgres"])
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .output()
        .expect("initdb");
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    let log_path = format!("/tmp/rvbbit-ro-{pg_port}.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let postgres = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ulimit -s 65520; exec \"{bin}\" -D \"{data}\" -k /tmp -p {pg_port} -c listen_addresses=127.0.0.1 -c io_method=sync -c max_stack_depth=60000",
            bin = format!(
                "{}/postgres",
                std::env::var("CARGO_MANIFEST_DIR").unwrap().replace("/crates/rvbbit/hutch_mcp", "/target/fast-profile")
            ),
        ))
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .env("PGRUST_TZDIR", "/usr/share/zoneinfo")
        .env("RUST_MIN_STACK", "33554432")
        .env("RVBBIT_HUTCH_URL", format!("http://127.0.0.1:{hutch_port}"))
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("postgres");
    let postgres = ChildGuard(postgres);
    let mut up = false;
    for _ in 0..80 {
        if Command::new(format!("{}/psql", pg_bin()))
            .args(["-h", "/tmp", "-p", &pg_port.to_string(), "-U", "postgres", "-c", "SELECT 1"])
            .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            up = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(up, "postgres did not start:\n{}", std::fs::read_to_string(&log_path).unwrap_or_default());

    let script = format!("/tmp/rvbbit-ro-{pg_port}.sql");
    let mut file = std::fs::File::create(&script).unwrap();
    file.write_all(
        b"\
CREATE SCHEMA rvbbit;
SELECT 'hello alpha' MEANS 'alpha';
SELECT to_regclass('rvbbit.semfn') IS NOT NULL;
SELECT to_regclass('rvbbit.semop') IS NOT NULL;
SELECT to_regclass('rvbbit.workflow') IS NOT NULL;

BEGIN READ ONLY;
SELECT 'hello alpha' MEANS 'alpha';
SELECT 'hello alpha' ABOUT 'alpha';
SELECT rvbbit.means_score('hello alpha', 'alpha');
SELECT CLASSIFY('a small bird' INTO 'bird', 'plane');
SELECT rvbbit.extract_typed('text', 'hello alpha', 'kept');
SELECT rvbbit.semantic_similarity('alpha', 'alpha');
COMMIT;
",
    )
    .unwrap();
    psql_file(pg_port, &script);

    let means = psql(pg_port, "SELECT 'hello alpha' MEANS 'alpha'");
    assert_eq!(means, "t", "{means}");
    assert_eq!(psql(pg_port, "SELECT to_regclass('rvbbit.semfn') IS NOT NULL"), "t");

    let ro = format!("/tmp/rvbbit-ro-{pg_port}-tx.sql");
    std::fs::write(
        &ro,
        "\
BEGIN READ ONLY;
SELECT 'hello alpha' MEANS 'alpha' AS means;
SELECT 'hello alpha' ABOUT 'alpha' AS about;
SELECT rvbbit.means_score('hello alpha', 'alpha') AS score;
SELECT CLASSIFY('a small bird' INTO 'bird', 'plane') AS label;
SELECT rvbbit.extract_typed('text', 'hello alpha', 'kept') AS extracted;
SELECT rvbbit.semantic_similarity('alpha', 'alpha') AS sim;
COMMIT;
",
    )
    .unwrap();
    psql_file(pg_port, &ro);
    assert_eq!(psql(pg_port, "SELECT CLASSIFY('a small bird' INTO 'bird', 'plane')"), "bird");

    assert_eq!(
        psql(pg_port, "SELECT has_schema_privilege('rvbbit_semantic_user', 'rvbbit', 'USAGE')"),
        "t"
    );
    for rel in ["semop", "semfn", "backend", "model", "decision_tree", "workflow"] {
        let privs = psql(
            pg_port,
            &format!("SELECT has_table_privilege('rvbbit_semantic_user', 'rvbbit.{rel}', 'SELECT')"),
        );
        assert_eq!(privs, "t", "{rel}");
    }
    assert_eq!(
        psql(pg_port, "SELECT has_table_privilege('rvbbit_semantic_user', 'rvbbit.my_calls', 'SELECT')"),
        "t"
    );

    psql(
        pg_port,
        "CREATE ROLE sem_reader NOLOGIN; CREATE ROLE sem_other NOLOGIN; \
         GRANT rvbbit_semantic_user TO sem_reader, sem_other;",
    );
    let as_reader = format!("/tmp/rvbbit-ro-{pg_port}-reader.sql");
    std::fs::write(
        &as_reader,
        "\
BEGIN;
SET LOCAL ROLE sem_reader;
SELECT count(*) > 0 FROM rvbbit.semfn;
SELECT 'reader alpha' MEANS 'alpha';
SELECT CLASSIFY('a small bird' INTO 'bird', 'plane');
SELECT rvbbit.extract_typed('text', 'reader note', 'kept');
SELECT rvbbit.rerank('a small bird', 'bird');
SELECT count(*) FROM rvbbit.my_calls;
COMMIT;
",
    )
    .unwrap();
    psql_file(pg_port, &as_reader);
    let mine = psql(
        pg_port,
        "BEGIN; SET LOCAL ROLE sem_reader; SELECT count(*) FROM rvbbit.my_calls; COMMIT;",
    );
    // psql -c runs one command string; a multi-statement -c returns the last result.
    assert!(mine.parse::<i64>().unwrap_or(0) >= 1, "{mine}");
    let other = psql(
        pg_port,
        "BEGIN; SET LOCAL ROLE sem_other; SELECT count(*) FROM rvbbit.my_calls; COMMIT;",
    );
    assert_eq!(other, "0", "{other}");
    let owner = psql(pg_port, "SELECT count(*) FROM rvbbit.calls WHERE tenant = 'sem_reader'");
    assert!(owner.parse::<i64>().unwrap_or(0) >= 1, "{owner}");
    let attributed = psql(
        pg_port,
        "SELECT count(*) FROM rvbbit.calls \
         WHERE tenant = 'sem_reader' AND role = 'sem_reader' \
           AND op IN ('predict', 'classify', 'extract', 'rerank')",
    );
    assert!(
        attributed.parse::<i64>().unwrap_or(0) >= 4,
        "provider calls were not attributed to the role: {attributed}"
    );
    let local = psql(
        pg_port,
        "SELECT count(*) FROM rvbbit.calls WHERE tenant = 'local' AND op IN ('predict', 'classify', 'extract', 'rerank')",
    );
    assert_eq!(local, "0", "worker meters fell back to tenant local: {local}");
    let mine_role = psql(
        pg_port,
        "BEGIN; SET LOCAL ROLE sem_reader; \
         SELECT count(*) FROM rvbbit.my_calls WHERE role = current_user AND tenant = current_user; \
         COMMIT;",
    );
    assert!(mine_role.parse::<i64>().unwrap_or(0) >= 4, "{mine_role}");

    let denied = Command::new(format!("{}/psql", pg_bin()))
        .args([
            "-h",
            "/tmp",
            "-p",
            &pg_port.to_string(),
            "-U",
            "postgres",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            "BEGIN; SET LOCAL ROLE sem_reader; SELECT count(*) FROM rvbbit.calls; COMMIT;",
        ])
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .output()
        .unwrap();
    assert!(!denied.status.success(), "mapped role must not read the full call log");

    psql(
        pg_port,
        "CREATE TABLE sem_rows (body text); INSERT INTO sem_rows SELECT 'a small bird' FROM generate_series(1, 8);",
    );
    let plan = psql(
        pg_port,
        "EXPLAIN (ANALYZE, COSTS ON) SELECT count(*) FROM (SELECT CLASSIFY(body INTO 'bird', 'plane') FROM sem_rows) s",
    );
    assert!(
        plan.contains("Actual HTTP Batches: 1"),
        "classify under count(*) was not one batch:\n{plan}"
    );
    assert!(
        !plan.contains("Actual HTTP Batches: 8"),
        "classify under count(*) ran once per row:\n{plan}"
    );
    assert!(plan.to_ascii_lowercase().contains("aggregate"), "{plan}");
    assert!(
        plan.contains("Semantic Provider Calls") || plan.contains("Semantic HTTP Batches"),
        "no semantic node under the aggregate:\n{plan}"
    );

    // A FILTER the planner does not price still calls the provider.
    // last_actual is that run, not the EXPLAIN estimate.
    psql(
        pg_port,
        "CREATE TABLE filt (x text); \
         INSERT INTO filt SELECT 'note-' || g::text FROM generate_series(1, 12) g;",
    );
    let actual = psql(
        pg_port,
        "BEGIN; \
         SET LOCAL semantic.capture_actual = 'on'; \
         SELECT count(*) FILTER (WHERE x MEANS 'alpha') FROM filt; \
         SET LOCAL semantic.capture_actual = 'off'; \
         SELECT rvbbit.last_actual(); \
         COMMIT;",
    );
    let actual = actual.lines().last().unwrap_or("").trim().to_string();
    let (calls, usd) = actual.split_once(',').unwrap_or_else(|| panic!("last_actual: {actual}"));
    let calls: f64 = calls.trim().parse().unwrap_or_else(|_| panic!("calls: {actual}"));
    let usd: f64 = usd.trim().parse().unwrap_or_else(|_| panic!("usd: {actual}"));
    assert_eq!(calls, 12.0, "FILTER MEANS actuals were {actual}");
    let expect = 12.0 * 300.0 * 0.042 / 1_000_000.0;
    assert!((usd - expect).abs() < 1e-9, "cost {usd} vs {expect} ({actual})");

    // Setup already created the catalog. Bootstrap after that must grant
    // rvbbit_auth, and a second run must not notice about existing columns.
    psql(pg_port, "CREATE ROLE rvbbit_auth LOGIN NOSUPERUSER NOINHERIT;");
    let before = psql(pg_port, "SELECT has_table_privilege('rvbbit_auth', 'rvbbit.semfn', 'SELECT')");
    assert_eq!(before, "f", "semfn was granted before bootstrap: {before}");
    let bootstrap = include_str!("../sql/bootstrap.sql");
    let boot_path = format!("/tmp/rvbbit-ro-{pg_port}-boot.sql");
    std::fs::write(&boot_path, bootstrap).unwrap();
    let first = psql_file_output(pg_port, &boot_path);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let after = psql(pg_port, "SELECT has_table_privilege('rvbbit_auth', 'rvbbit.semfn', 'SELECT')");
    assert_eq!(after, "t", "{after}");
    assert_eq!(
        psql(pg_port, "BEGIN; SET LOCAL ROLE rvbbit_auth; SELECT count(*) >= 0 FROM rvbbit.semfn; COMMIT;"),
        "t"
    );
    psql(
        pg_port,
        "BEGIN; SET LOCAL ROLE rvbbit_auth; \
         INSERT INTO rvbbit.oauth_client (client_id, secret_hash, redirect_uris) \
         VALUES ('order-b', NULL, '[]'::jsonb); COMMIT;",
    );
    psql(pg_port, "REVOKE SELECT ON rvbbit.semfn FROM rvbbit_auth;");
    psql(
        pg_port,
        "BEGIN; SET LOCAL ROLE rvbbit_auth; \
         INSERT INTO rvbbit.oauth_client (client_id, secret_hash, redirect_uris) \
         VALUES ('order-b-noread', NULL, '[]'::jsonb); COMMIT;",
    );
    let denied = Command::new(format!("{}/psql", pg_bin()))
        .args([
            "-h",
            "/tmp",
            "-p",
            &pg_port.to_string(),
            "-U",
            "postgres",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            "BEGIN; SET LOCAL ROLE rvbbit_auth; SELECT count(*) FROM rvbbit.semfn; COMMIT;",
        ])
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .output()
        .unwrap();
    assert!(!denied.status.success(), "revoked semfn should be unreadable");
    assert!(
        String::from_utf8_lossy(&denied.stderr).to_ascii_lowercase().contains("permission denied"),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );
    let second = psql_file_output(pg_port, &boot_path);
    assert!(second.status.success(), "{}", String::from_utf8_lossy(&second.stderr));
    assert!(
        !column_skip_notice(&String::from_utf8_lossy(&second.stderr)),
        "bootstrap re-run noticed a column: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !column_skip_notice(&log),
        "catalog re-run noticed a column:\n{log}"
    );

    drop(postgres);
    let _ = std::fs::remove_dir_all(&data);
}

fn psql_file_output(port: u16, path: &str) -> std::process::Output {
    Command::new(format!("{}/psql", pg_bin()))
        .args(["-h", "/tmp", "-p", &port.to_string(), "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-f", path])
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .output()
        .expect("psql")
}

fn column_skip_notice(text: &str) -> bool {
    text.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("column") && lower.contains("already exists, skipping")
    })
}

/// Bootstrap before rvbbit setup. Default privileges plus the setup grants
/// let rvbbit_auth read semfn, and OAuth does not need that read.
#[test]
fn bootstrap_before_setup_grants_auth() {
    let pg_port = free_port();
    let hutch_port = free_port();
    assert_ne!(pg_port, 55432);
    assert_ne!(hutch_port, 8099);
    let listener = TcpListener::bind(("127.0.0.1", hutch_port)).unwrap();
    thread::spawn(move || {
        let cfg = hutch_mock::Config {
            latency: Duration::ZERO,
            fail_every: 0,
            timeout_every: 0,
            expect_auth: None,
        };
        let _ = hutch_mock::serve_listener(listener, cfg);
    });
    let data = format!("/tmp/rvbbit-boot-{pg_port}");
    let _ = std::fs::remove_dir_all(&data);
    let init = Command::new(format!("{}/initdb", pg_bin()))
        .args(["-D", &data, "--no-locale", "--encoding=UTF8", "--auth-local=trust", "-U", "postgres"])
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .output()
        .expect("initdb");
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    let log_path = format!("/tmp/rvbbit-boot-{pg_port}.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let postgres = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ulimit -s 65520; exec \"{bin}\" -D \"{data}\" -k /tmp -p {pg_port} -c listen_addresses=127.0.0.1 -c io_method=sync -c max_stack_depth=60000",
            bin = format!(
                "{}/postgres",
                std::env::var("CARGO_MANIFEST_DIR").unwrap().replace("/crates/rvbbit/hutch_mcp", "/target/fast-profile")
            ),
        ))
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .env("PGRUST_TZDIR", "/usr/share/zoneinfo")
        .env("RUST_MIN_STACK", "33554432")
        .env("RVBBIT_HUTCH_URL", format!("http://127.0.0.1:{hutch_port}"))
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("postgres");
    let postgres = ChildGuard(postgres);
    let mut up = false;
    for _ in 0..80 {
        if Command::new(format!("{}/psql", pg_bin()))
            .args(["-h", "/tmp", "-p", &pg_port.to_string(), "-U", "postgres", "-c", "SELECT 1"])
            .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            up = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(up, "postgres did not start:\n{}", std::fs::read_to_string(&log_path).unwrap_or_default());

    psql(pg_port, "CREATE ROLE rvbbit_auth LOGIN NOSUPERUSER NOINHERIT;");
    let boot_path = format!("/tmp/rvbbit-boot-{pg_port}.sql");
    std::fs::write(&boot_path, include_str!("../sql/bootstrap.sql")).unwrap();
    let loaded = psql_file_output(pg_port, &boot_path);
    assert!(loaded.status.success(), "{}", String::from_utf8_lossy(&loaded.stderr));
    assert_eq!(psql(pg_port, "SELECT 'hello alpha' MEANS 'alpha'"), "t");
    assert_eq!(psql(pg_port, "SELECT has_table_privilege('rvbbit_auth', 'rvbbit.semfn', 'SELECT')"), "t");
    assert_eq!(
        psql(pg_port, "BEGIN; SET LOCAL ROLE rvbbit_auth; SELECT count(*) >= 0 FROM rvbbit.semfn; COMMIT;"),
        "t"
    );
    psql(
        pg_port,
        "BEGIN; SET LOCAL ROLE rvbbit_auth; \
         INSERT INTO rvbbit.oauth_client (client_id, secret_hash, redirect_uris) \
         VALUES ('order-a', NULL, '[]'::jsonb); COMMIT;",
    );
    let again = psql_file_output(pg_port, &boot_path);
    assert!(again.status.success(), "{}", String::from_utf8_lossy(&again.stderr));
    assert!(
        !column_skip_notice(&String::from_utf8_lossy(&again.stderr)),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    drop(postgres);
    let _ = std::fs::remove_dir_all(&data);
}

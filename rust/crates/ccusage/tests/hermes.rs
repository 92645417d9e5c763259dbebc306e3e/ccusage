use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use ccusage_test_support::Fixture;
use serde_json::Value;

const REPORT_MODES: [&str; 4] = ["daily", "weekly", "monthly", "session"];

fn create_state_db(path: &Path, sessions: &[(&str, i64, i64)]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let db = sqlite::open(path).unwrap();
    db.execute(
        "
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                model TEXT,
                started_at REAL NOT NULL,
                message_count INTEGER DEFAULT 0,
                input_tokens INTEGER DEFAULT 0,
                output_tokens INTEGER DEFAULT 0,
                cache_read_tokens INTEGER DEFAULT 0,
                cache_write_tokens INTEGER DEFAULT 0,
                reasoning_tokens INTEGER DEFAULT 0,
                billing_provider TEXT,
                estimated_cost_usd REAL,
                actual_cost_usd REAL
            );
        ",
    )
    .unwrap();

    for (session_id, input_tokens, output_tokens) in sessions {
        insert_session(&db, session_id, *input_tokens, *output_tokens);
    }
}

fn insert_session(
    db: &sqlite::Connection,
    session_id: &str,
    input_tokens: i64,
    output_tokens: i64,
) {
    let mut statement = db
        .prepare(
            "
                INSERT INTO sessions (
                    id, source, model, started_at, message_count,
                    input_tokens, output_tokens
                ) VALUES (?1, 'cli', 'claude-sonnet-4-20250514', 1750000000, 1, ?2, ?3)
            ",
        )
        .unwrap();
    statement.bind((1, session_id)).unwrap();
    statement.bind((2, input_tokens)).unwrap();
    statement.bind((3, output_tokens)).unwrap();
    statement.next().unwrap();
}

fn run_hermes_report(
    fixture: &Fixture,
    mode: &str,
    json: bool,
    hermes_home: Option<&str>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ccusage"));
    command
        .env("HOME", fixture.root())
        .env_remove("HERMES_HOME")
        .env("LOG_LEVEL", "0")
        .args([
            "hermes",
            mode,
            "--offline",
            "--timezone",
            "UTC",
            "--no-color",
        ]);
    if json {
        command.arg("--json");
    }
    if let Some(hermes_home) = hermes_home {
        command.env("HERMES_HOME", hermes_home);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "ccusage hermes {mode} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn report_rows<'a>(report: &'a Value, mode: &str) -> &'a [Value] {
    let key = if mode == "session" { "sessions" } else { mode };
    report[key].as_array().unwrap()
}

fn assert_totals(report: &Value, input_tokens: u64, output_tokens: u64) {
    assert_eq!(report["totals"]["inputTokens"].as_u64(), Some(input_tokens));
    assert_eq!(
        report["totals"]["outputTokens"].as_u64(),
        Some(output_tokens)
    );
}

#[test]
fn cli_json_and_table_reports_preserve_profile_scoped_session_ids() {
    let fixture = Fixture::new();
    create_state_db(&fixture.path(".hermes/state.db"), &[("collision", 100, 10)]);
    create_state_db(
        &fixture.path(".hermes/profiles/alpha/state.db"),
        &[("collision", 200, 20)],
    );
    create_state_db(
        &fixture.path(".hermes/profiles/beta/state.db"),
        &[("collision", 300, 30)],
    );

    for mode in REPORT_MODES {
        let output = run_hermes_report(&fixture, mode, true, None);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        match mode {
            "daily" => insta::assert_json_snapshot!("hermes_daily_json", report),
            "weekly" => insta::assert_json_snapshot!("hermes_weekly_json", report),
            "monthly" => insta::assert_json_snapshot!("hermes_monthly_json", report),
            "session" => insta::assert_json_snapshot!("hermes_session_json", report),
            _ => unreachable!(),
        }

        let output = run_hermes_report(&fixture, mode, false, None);
        let table = String::from_utf8(output.stdout).unwrap();
        match mode {
            "daily" => insta::assert_snapshot!("hermes_daily_table", table),
            "weekly" => insta::assert_snapshot!("hermes_weekly_table", table),
            "monthly" => insta::assert_snapshot!("hermes_monthly_table", table),
            "session" => insta::assert_snapshot!("hermes_session_table", table),
            _ => unreachable!(),
        }
    }
}

#[test]
fn cli_reads_automatic_profile_database_with_live_wal_sidecars() {
    let fixture = Fixture::new();
    let profile_db = fixture.path(".hermes/profiles/live/state.db");
    fs::create_dir_all(profile_db.parent().unwrap()).unwrap();
    create_state_db(&profile_db, &[]);
    let db = sqlite::open(&profile_db).unwrap();
    db.execute("PRAGMA journal_mode = WAL").unwrap();
    insert_session(&db, "live", 100, 10);

    assert!(fixture.path(".hermes/profiles/live/state.db-wal").is_file());
    let output = run_hermes_report(&fixture, "session", true, None);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_totals(&report, 100, 10);
    assert_eq!(
        report["sessions"][0]["sessionId"].as_str(),
        Some("live/live")
    );
}

#[cfg(unix)]
#[test]
fn cli_json_and_table_reports_deduplicate_same_database_aliases() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let first_root = fixture.path("first");
    let alias_root = fixture.path("alias");
    let first_db = first_root.join("state.db");
    let alias_db = alias_root.join("state.db");
    create_state_db(&first_db, &[("aliased", 100, 10)]);
    fs::create_dir_all(&alias_root).unwrap();
    symlink(&first_db, &alias_db).unwrap();
    let hermes_home = format!("{},{}", first_root.display(), alias_root.display());

    for mode in REPORT_MODES {
        let output = run_hermes_report(&fixture, mode, true, Some(&hermes_home));
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_totals(&report, 100, 10);
        let rows = report_rows(&report, mode);
        assert_eq!(rows.len(), 1);
        if mode == "session" {
            assert_eq!(rows[0]["sessionId"].as_str(), Some("aliased"));
        }

        let output = run_hermes_report(&fixture, mode, false, Some(&hermes_home));
        let table = String::from_utf8(output.stdout).unwrap();
        assert!(
            table.contains("100"),
            "{mode} table omitted the total:\n{table}"
        );
        if mode == "session" {
            assert_eq!(table.matches("aliased").count(), 1, "{table}");
        }
    }
}

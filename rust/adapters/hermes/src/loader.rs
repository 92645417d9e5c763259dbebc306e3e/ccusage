use std::{
    collections::HashSet,
    ffi::{CStr, CString},
    fs::File,
    marker::PhantomData,
    os::raw::c_int,
    path::{Path, PathBuf},
    ptr,
    sync::Arc,
};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use crate::{LoadedEntry, PricingMap, Result, cli::SharedArgs, read_files_parallel};

use super::{
    parser::{HermesEntry, SessionRow, read_session_row, to_loaded_entry},
    paths::{file_matches_path, hermes_state_db_paths, is_regular_non_symlink},
};

const SESSION_QUERY: &str = "
    SELECT
        id,
        model,
        billing_provider,
        started_at,
        message_count,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
        estimated_cost_usd,
        actual_cost_usd
    FROM sessions
    WHERE model IS NOT NULL
        AND TRIM(model) != ''
";

struct SqliteConnection {
    raw: *mut sqlite::ffi::sqlite3,
    _file: Arc<File>,
}

struct SqliteStatement<'connection> {
    raw: *mut sqlite::ffi::sqlite3_stmt,
    _connection: PhantomData<&'connection SqliteConnection>,
}

#[cfg(unix)]
fn canonical_path_without_final_component(path: &Path) -> Option<PathBuf> {
    // SQLite's no-follow VFS rejects parent symlinks, while macOS commonly exposes /var this way.
    // Canonicalizing only the parent preserves the database basename and its sidecar namespace.
    Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?))
}

impl SqliteConnection {
    fn open(path: &Path, file: Arc<File>, no_follow: bool) -> Option<Self> {
        // SQLite derives live WAL and SHM filenames from the pathname, so use the discovered path
        // for profiles and retain the descriptor only for a path that is no longer safe to follow.
        if no_follow && !is_regular_non_symlink(path) {
            #[cfg(unix)]
            return Self::open_descriptor(file);
            #[cfg(not(unix))]
            return None;
        }

        let flags = sqlite::ffi::SQLITE_OPEN_READONLY
            | if no_follow {
                sqlite::ffi::SQLITE_OPEN_NOFOLLOW
            } else {
                0
            };
        #[cfg(unix)]
        let sqlite_path = if no_follow {
            canonical_path_without_final_component(path)
        } else {
            Some(path.to_path_buf())
        };
        #[cfg(not(unix))]
        let sqlite_path = Some(path.to_path_buf());
        let Some(sqlite_path) = sqlite_path else {
            #[cfg(unix)]
            return Self::open_descriptor(file);
            #[cfg(not(unix))]
            return None;
        };
        let connection = sqlite_path
            .to_str()
            .and_then(|path| Self::open_path(path, flags, Arc::clone(&file)));
        if !no_follow {
            return connection;
        }
        if let Some(connection) = connection
            && is_regular_non_symlink(path)
            && file_matches_path(path, &file)
        {
            return Some(connection);
        }

        #[cfg(unix)]
        return Self::open_descriptor(file);
        #[cfg(not(unix))]
        None
    }

    #[cfg(unix)]
    fn open_descriptor(file: Arc<File>) -> Option<Self> {
        Self::open_path(
            &format!("/dev/fd/{}", file.as_raw_fd()),
            sqlite::ffi::SQLITE_OPEN_READONLY,
            file,
        )
    }

    fn open_path(path: &str, flags: c_int, file: Arc<File>) -> Option<Self> {
        let path = CString::new(path).ok()?;
        let mut raw = ptr::null_mut();
        let code =
            unsafe { sqlite::ffi::sqlite3_open_v2(path.as_ptr(), &mut raw, flags, ptr::null()) };
        if code != sqlite::ffi::SQLITE_OK {
            // SQLite can return a partially initialized handle on failure.
            if !raw.is_null() {
                unsafe { sqlite::ffi::sqlite3_close(raw) };
            }
            return None;
        }
        Some(Self { raw, _file: file })
    }

    fn prepare(&self, query: &str) -> Option<SqliteStatement<'_>> {
        let query = CString::new(query).ok()?;
        let mut raw = ptr::null_mut();
        // The statement borrows this live connection and the query buffer only for preparation.
        let code = unsafe {
            sqlite::ffi::sqlite3_prepare_v2(self.raw, query.as_ptr(), -1, &mut raw, ptr::null_mut())
        };
        if code != sqlite::ffi::SQLITE_OK {
            if !raw.is_null() {
                unsafe { sqlite::ffi::sqlite3_finalize(raw) };
            }
            return None;
        }
        Some(SqliteStatement {
            raw,
            _connection: PhantomData,
        })
    }
}

impl Drop for SqliteConnection {
    fn drop(&mut self) {
        // The connection owns the native handle until it is explicitly closed.
        unsafe { sqlite::ffi::sqlite3_close(self.raw) };
    }
}

impl SqliteStatement<'_> {
    fn next(&mut self) -> c_int {
        // SQLite requires stepping a prepared statement through its native handle.
        unsafe { sqlite::ffi::sqlite3_step(self.raw) }
    }
}

impl Drop for SqliteStatement<'_> {
    fn drop(&mut self) {
        // Finalizing releases the statement before its borrowed connection can be dropped.
        unsafe { sqlite::ffi::sqlite3_finalize(self.raw) };
    }
}

impl SessionRow for SqliteStatement<'_> {
    fn read_text(&self, index: usize) -> Option<String> {
        let value = unsafe { sqlite::ffi::sqlite3_column_text(self.raw, index as c_int) };
        if value.is_null() {
            return None;
        }
        // SQLite returns a nul-terminated UTF-8-compatible buffer for text columns.
        Some(
            unsafe { CStr::from_ptr(value.cast()) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn read_integer(&self, index: usize) -> Option<i64> {
        Some(unsafe { sqlite::ffi::sqlite3_column_int64(self.raw, index as c_int) })
    }

    fn read_real(&self, index: usize) -> Option<f64> {
        Some(unsafe { sqlite::ffi::sqlite3_column_double(self.raw, index as c_int) })
    }
}

pub fn load_entries(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(
        crate::progress::UsageLoadAgent("Hermes"),
        shared.json,
        || load_entries_inner(shared, pricing),
    )
}

fn load_entries_inner(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    let tz = crate::parse_tz(shared.timezone.as_deref());
    let dbs = hermes_state_db_paths()?;
    let db_paths = dbs.iter().map(|db| db.path.clone()).collect::<Vec<_>>();
    // Load each state database in parallel (a fresh read-only connection per DB),
    // then run the sequential session dedup over the original path order so the
    // surviving session matches the single-threaded read.
    let loaded = read_files_parallel(&db_paths, shared.single_thread, |db_path| {
        let db = dbs
            .iter()
            .find(|candidate| candidate.path == db_path)
            .expect("every database path has discovery metadata");
        (
            db.clone(),
            load_state_db_entries(&db.path, Arc::clone(&db.file), db.no_follow, shared),
        )
    });
    let mut entries = Vec::new();
    let mut seen_sessions = HashSet::new();
    for (db, db_entries) in loaded {
        for entry in db_entries {
            if !seen_sessions.insert((db.database_identity.clone(), entry.session_id.clone())) {
                continue;
            }
            let session_label = db.profile_name.as_ref().map(|profile_name| {
                format!("{}/{}", profile_name.to_string_lossy(), entry.session_id)
            });
            let session_key = db.database_identity.session_key(&entry.session_id);
            entries.push(to_loaded_entry(
                entry,
                tz.as_ref(),
                pricing,
                &session_key,
                session_label.as_deref(),
            ));
        }
    }
    entries.sort_by_key(|entry| entry.timestamp);
    Ok(entries)
}

fn load_state_db_entries(
    db_path: &Path,
    file: Arc<File>,
    no_follow: bool,
    shared: &SharedArgs,
) -> Vec<HermesEntry> {
    let Some(connection) = SqliteConnection::open(db_path, file, no_follow) else {
        crate::debug_log(
            shared,
            format!(
                "Failed to open Hermes state database: {}",
                db_path.display()
            ),
        );
        return Vec::new();
    };
    let Some(mut statement) = connection.prepare(SESSION_QUERY) else {
        crate::debug_log(
            shared,
            format!(
                "Failed to read Hermes state database: {}",
                db_path.display()
            ),
        );
        return Vec::new();
    };
    let mut entries = Vec::new();
    loop {
        match statement.next() {
            sqlite::ffi::SQLITE_ROW => {
                if let Some(entry) = read_session_row(&statement) {
                    entries.push(entry);
                }
            }
            sqlite::ffi::SQLITE_DONE => break,
            _ => {
                crate::debug_log(
                    shared,
                    format!(
                        "Failed to query Hermes state database: {}",
                        db_path.display()
                    ),
                );
                break;
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Arc};

    use super::*;
    use crate::cli::AgentReportKind;
    use ccusage_test_support::EnvVarsGuard;
    use ccusage_test_support::fs_fixture;

    fn create_state_db(path: &Path) {
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
    }

    fn insert_session(
        path: &Path,
        session_id: &str,
        started_at: f64,
        input_tokens: i64,
        output_tokens: i64,
    ) {
        let db = sqlite::open(path).unwrap();
        let mut statement = db
            .prepare(
                "
                    INSERT INTO sessions (
                        id, source, model, started_at, message_count, input_tokens, output_tokens
                    ) VALUES (?1, 'cli', 'claude-sonnet-4-20250514', ?2, 1, ?3, ?4)
                ",
            )
            .unwrap();
        statement.bind((1, session_id)).unwrap();
        statement.bind((2, started_at)).unwrap();
        statement.bind((3, input_tokens)).unwrap();
        statement.bind((4, output_tokens)).unwrap();
        statement.next().unwrap();
    }

    fn test_shared_args() -> SharedArgs {
        SharedArgs {
            single_thread: true,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        }
    }

    #[test]
    fn keeps_same_session_ids_distinct_across_databases_in_every_report_mode() {
        let fixture = fs_fixture!({});
        let default_db = fixture.path(".hermes/state.db");
        let profile_db = fixture.path(".hermes/profiles/work/state.db");
        let _ = fixture.create_dir_all(".hermes/profiles/work");
        create_state_db(&default_db);
        create_state_db(&profile_db);
        insert_session(&default_db, "default-only", 1_750_000_000.0, 100, 10);
        insert_session(&default_db, "shared", 1_750_000_001.0, 200, 10);
        insert_session(&profile_db, "profile-only", 1_750_000_002.0, 300, 10);
        insert_session(&profile_db, "shared", 1_750_000_003.0, 400, 10);
        let _env_guard = EnvVarsGuard::set_many([
            ("HOME", Some(fixture.root().as_os_str().to_os_string())),
            ("HERMES_HOME", None),
        ]);
        let pricing = PricingMap::load_embedded();
        let shared = test_shared_args();

        let entries = load_entries_inner(&shared, &pricing).unwrap();

        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.data.session_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["default-only", "shared", "work/profile-only", "work/shared"]
        );
        assert_ne!(entries[1].session_id, entries[3].session_id);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.data.message.usage.input_tokens)
                .sum::<u64>(),
            1000
        );

        for (kind, rows_key, expected_rows) in [
            (AgentReportKind::Daily, "daily", 1),
            (AgentReportKind::Weekly, "weekly", 1),
            (AgentReportKind::Monthly, "monthly", 1),
            (AgentReportKind::Session, "sessions", 4),
        ] {
            let rows = crate::summarize_entries(&entries, kind).unwrap();
            let report = crate::report_from_rows(&rows, kind);
            assert_eq!(report[rows_key].as_array().unwrap().len(), expected_rows);
            assert_eq!(report["totals"]["inputTokens"].as_u64(), Some(1000));
            assert_eq!(report["totals"]["outputTokens"].as_u64(), Some(40));
        }
    }

    #[test]
    fn explicit_homes_load_only_their_root_databases() {
        let fixture = fs_fixture!({});
        let first_db = fixture.path("first/state.db");
        let second_db = fixture.path("second/state.db");
        let profile_db = fixture.path("first/profiles/ignored/state.db");
        let _ = fixture.create_dir_all("first/profiles/ignored");
        let _ = fixture.create_dir_all("second");
        create_state_db(&first_db);
        create_state_db(&second_db);
        create_state_db(&profile_db);
        insert_session(&first_db, "first", 1_750_000_000.0, 100, 10);
        insert_session(&second_db, "second", 1_750_000_001.0, 200, 20);
        insert_session(&profile_db, "profile", 1_750_000_002.0, 400, 40);
        let homes = [fixture.path("first"), fixture.path("second")]
            .map(|path| path.display().to_string())
            .join(",");
        let _env_guard = EnvVarsGuard::set_many([("HERMES_HOME", Some(homes.into()))]);
        let pricing = PricingMap::load_embedded();
        let shared = test_shared_args();

        let entries = load_entries_inner(&shared, &pricing).unwrap();

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.data.session_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn skips_missing_and_invalid_state_databases() {
        let fixture = fs_fixture!({
            ".hermes/profiles/broken/state.db": "not a SQLite database",
        });
        let default_db = fixture.path(".hermes/state.db");
        create_state_db(&default_db);
        insert_session(&default_db, "valid", 1_750_000_000.0, 100, 10);
        let _env_guard = EnvVarsGuard::set_many([
            ("HOME", Some(fixture.root().as_os_str().to_os_string())),
            ("HERMES_HOME", None),
        ]);
        let pricing = PricingMap::load_embedded();
        let shared = test_shared_args();

        let entries = load_entries_inner(&shared, &pricing).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.session_id.as_deref(), Some("valid"));
    }

    #[test]
    fn loads_billable_hermes_sessions_from_state_db() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("state.db");
        create_state_db(&db_path);
        let db = sqlite::open(&db_path).unwrap();
        let mut statement = db
            .prepare(
                "
                    INSERT INTO sessions (
                        id, source, model, started_at, message_count,
                        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                        billing_provider, estimated_cost_usd, actual_cost_usd
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                ",
            )
            .unwrap();
        statement.bind((1, "session-1")).unwrap();
        statement.bind((2, "cli")).unwrap();
        statement.bind((3, "claude-sonnet-4-20250514")).unwrap();
        statement.bind((4, 1_750_000_000.25)).unwrap();
        statement.bind((5, 42_i64)).unwrap();
        statement.bind((6, 1200_i64)).unwrap();
        statement.bind((7, 300_i64)).unwrap();
        statement.bind((8, 50_i64)).unwrap();
        statement.bind((9, 20_i64)).unwrap();
        statement.bind((10, 10_i64)).unwrap();
        statement.bind((11, "anthropic")).unwrap();
        statement.bind((12, 0.12)).unwrap();
        statement.bind((13, 0.34)).unwrap();
        statement.next().unwrap();

        let pricing = PricingMap::load_embedded();
        let shared = SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let tz = crate::parse_tz(shared.timezone.as_deref());
        let file = Arc::new(File::open(&db_path).unwrap());
        let entries = load_state_db_entries(&db_path, file, false, &shared)
            .into_iter()
            .map(|entry| to_loaded_entry(entry, tz.as_ref(), &pricing, "session-1", None))
            .collect::<Vec<_>>();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].date, "2025-06-15");
        assert_eq!(entries[0].data.session_id.as_deref(), Some("session-1"));
        assert_eq!(
            entries[0].model.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
        assert_eq!(entries[0].data.message.usage.input_tokens, 1200);
        assert_eq!(entries[0].data.message.usage.output_tokens, 300);
        assert_eq!(
            entries[0].data.message.usage.cache_creation_input_tokens,
            20
        );
        assert_eq!(entries[0].data.message.usage.cache_read_input_tokens, 50);
        assert_eq!(entries[0].extra_total_tokens, 10);
        assert_eq!(entries[0].message_count, Some(42));
        assert_eq!(entries[0].cost, 0.34);
    }

    #[cfg(unix)]
    #[test]
    fn reads_the_database_opened_during_profile_discovery_after_path_replacement() {
        use std::os::unix::fs::symlink;

        let fixture = fs_fixture!({});
        let profile_db = fixture.path(".hermes/profiles/work/state.db");
        let replacement_db = fixture.path("replacement.db");
        let _ = fixture.create_dir_all(".hermes/profiles/work");
        create_state_db(&profile_db);
        create_state_db(&replacement_db);
        insert_session(&profile_db, "original", 1_750_000_000.0, 100, 10);
        insert_session(&replacement_db, "replacement", 1_750_000_001.0, 200, 20);
        let _env_guard = EnvVarsGuard::set_many([
            ("HOME", Some(fixture.root().as_os_str().to_os_string())),
            ("HERMES_HOME", None),
        ]);
        let profile = hermes_state_db_paths()
            .unwrap()
            .into_iter()
            .find(|database| database.profile_name.as_deref() == Some(std::ffi::OsStr::new("work")))
            .unwrap();

        let moved_db = fixture.path("original.db");
        fs::rename(&profile.path, &moved_db).unwrap();
        symlink(&replacement_db, &profile.path).unwrap();

        let entries = load_state_db_entries(
            &profile.path,
            Arc::clone(&profile.file),
            profile.no_follow,
            &test_shared_args(),
        );

        assert_eq!(
            entries
                .into_iter()
                .map(|entry| entry.session_id)
                .collect::<Vec<_>>(),
            vec!["original"]
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn rejects_an_automatic_load_from_a_different_open_file() {
        let fixture = fs_fixture!({});
        let opened_path = fixture.path("opened.db");
        let mutable_path = fixture.path("mutable.db");
        create_state_db(&opened_path);
        create_state_db(&mutable_path);
        insert_session(&mutable_path, "mutable", 1_750_000_000.0, 100, 10);

        let entries = load_state_db_entries(
            &mutable_path,
            Arc::new(File::open(&opened_path).unwrap()),
            true,
            &test_shared_args(),
        );

        assert!(entries.is_empty());
    }
}

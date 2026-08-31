use std::{
    collections::HashSet,
    env,
    ffi::{CString, OsStr, OsString},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(unix)]
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
};

#[cfg(windows)]
use std::{ffi::c_void, os::windows::io::AsRawHandle};

use crate::Result;

const HERMES_HOME_ENV: &str = "HERMES_HOME";

#[cfg(windows)]
#[repr(C)]
struct WindowsFileTime {
    _low_date_time: u32,
    _high_date_time: u32,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsFileInformation {
    _file_attributes: u32,
    _creation_time: WindowsFileTime,
    _last_access_time: WindowsFileTime,
    _last_write_time: WindowsFileTime,
    volume_serial_number: u32,
    _file_size_high: u32,
    _file_size_low: u32,
    _number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "GetFileInformationByHandle"]
    fn get_file_information_by_handle(
        handle: *mut c_void,
        file_information: *mut WindowsFileInformation,
    ) -> i32;
}

#[cfg(windows)]
fn windows_file_identity(handle: *mut c_void) -> Option<(u64, u64)> {
    if handle.is_null() {
        return None;
    }
    let mut file_information = std::mem::MaybeUninit::<WindowsFileInformation>::uninit();
    let available =
        unsafe { get_file_information_by_handle(handle, file_information.as_mut_ptr()) };
    if available == 0 {
        return None;
    }
    let file_information = unsafe { file_information.assume_init() };
    let file_index = (u64::from(file_information.file_index_high) << 32)
        | u64::from(file_information.file_index_low);
    Some((u64::from(file_information.volume_serial_number), file_index))
}

#[derive(Clone, Debug)]
pub(super) struct HermesStateDbPath {
    pub(super) path: PathBuf,
    pub(super) database_identity: DatabaseIdentity,
    pub(super) file: Arc<File>,
    pub(super) no_follow: bool,
    pub(super) profile_name: Option<OsString>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum DatabaseIdentity {
    #[cfg(unix)]
    File { device: u64, inode: u64 },
    #[cfg(windows)]
    File { volume: u64, index: u64 },
    #[cfg(not(any(unix, windows)))]
    Unsupported,
}

impl DatabaseIdentity {
    pub(super) fn from_open_file(file: &File) -> Option<Self> {
        let metadata = file.metadata().ok()?;
        if !metadata.file_type().is_file() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            Some(Self::File {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            let (volume, index) = windows_file_identity(file.as_raw_handle())?;
            Some(Self::File { volume, index })
        }
        #[cfg(not(any(unix, windows)))]
        None
    }

    #[cfg(windows)]
    pub(super) fn from_windows_handle(handle: *mut c_void) -> Option<Self> {
        let (volume, index) = windows_file_identity(handle)?;
        Some(Self::File { volume, index })
    }

    pub(super) fn session_key(&self, session_id: &str) -> String {
        match self {
            #[cfg(unix)]
            Self::File { device, inode } => format!("{device}:{inode}\0{session_id}"),
            #[cfg(windows)]
            Self::File { volume, index } => format!("{volume}:{index}\0{session_id}"),
            #[cfg(not(any(unix, windows)))]
            Self::Unsupported => format!("unsupported\0{session_id}"),
        }
    }
}

pub(super) fn hermes_state_db_paths() -> Result<Vec<HermesStateDbPath>> {
    let (homes, discover_profiles) = if let Ok(paths) = env::var(HERMES_HOME_ENV) {
        (
            paths
                .split(',')
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>(),
            false,
        )
    } else {
        let home =
            crate::home::home_dir().ok_or_else(|| crate::cli_error("home directory is not set"))?;
        (vec![home.join(".hermes")], true)
    };
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for home in homes {
        add_state_db_path(&home.join("state.db"), None, &mut seen, &mut paths);
        if discover_profiles {
            for (path, profile_name, file) in profile_state_db_paths(&home) {
                add_opened_state_db_path(
                    &path,
                    file,
                    true,
                    Some(profile_name),
                    &mut seen,
                    &mut paths,
                );
            }
        }
    }
    Ok(paths)
}

fn profile_state_db_paths(home: &Path) -> Vec<(PathBuf, OsString, Arc<File>)> {
    let profiles_dir = home.join("profiles");
    let Ok(metadata) = fs::symlink_metadata(&profiles_dir) else {
        return Vec::new();
    };
    if !metadata.file_type().is_dir() {
        return Vec::new();
    }
    let Some(profiles_directory) = open_profiles_directory(&profiles_dir) else {
        return Vec::new();
    };

    let Ok(entries) = fs::read_dir(&profiles_dir) else {
        return Vec::new();
    };
    let mut profile_dirs = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            profile_dirs.push((entry.path(), entry.file_name()));
        }
    }
    profile_dirs.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    profile_dirs
        .into_iter()
        .filter_map(|(profile_dir, profile_name)| {
            let file = open_profile_state_db(&profiles_directory, &profile_name, &profile_dir)?;
            Some((profile_dir.join("state.db"), profile_name, Arc::new(file)))
        })
        .collect()
}

fn add_state_db_path(
    path: &Path,
    profile_name: Option<OsString>,
    seen: &mut HashSet<DatabaseIdentity>,
    paths: &mut Vec<HermesStateDbPath>,
) {
    let Some(file) = open_state_db_file(path, true) else {
        return;
    };
    add_opened_state_db_path(path, Arc::new(file), false, profile_name, seen, paths);
}

fn add_opened_state_db_path(
    path: &Path,
    file: Arc<File>,
    no_follow: bool,
    profile_name: Option<OsString>,
    seen: &mut HashSet<DatabaseIdentity>,
    paths: &mut Vec<HermesStateDbPath>,
) {
    let Some(database_identity) = DatabaseIdentity::from_open_file(&file) else {
        return;
    };
    if seen.insert(database_identity.clone()) {
        paths.push(HermesStateDbPath {
            path: path.to_path_buf(),
            database_identity,
            file,
            no_follow,
            profile_name,
        });
    }
}

fn open_state_db_file(path: &Path, follow_final_component: bool) -> Option<File> {
    if !follow_final_component && is_symlink(path) {
        return None;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    if !follow_final_component {
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).ok()?;
    if !follow_final_component {
        let metadata = fs::symlink_metadata(path).ok()?;
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_file()
            || !file_matches_path(path, &file)
        {
            return None;
        }
    }
    Some(file)
}

#[cfg(unix)]
fn open_profiles_directory(path: &Path) -> Option<File> {
    let file = open_path_with_flags(path, libc::O_RDONLY | libc::O_NOFOLLOW)?;
    file.metadata().ok()?.file_type().is_dir().then_some(file)
}

#[cfg(not(unix))]
fn open_profiles_directory(path: &Path) -> Option<File> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_dir() {
        return None;
    }
    let file = File::open(path).ok()?;
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_dir()
        || !file_matches_path(path, &file)
    {
        return None;
    }
    Some(file)
}

#[cfg(unix)]
fn open_profile_state_db(
    profiles_directory: &File,
    profile_name: &OsStr,
    _profile_dir: &Path,
) -> Option<File> {
    let profile_directory = open_at(
        profiles_directory,
        profile_name,
        libc::O_RDONLY | libc::O_NOFOLLOW,
    )?;
    if !profile_directory.metadata().ok()?.file_type().is_dir() {
        return None;
    }
    let state_db = open_at(
        &profile_directory,
        OsStr::new("state.db"),
        libc::O_RDONLY | libc::O_NOFOLLOW,
    )?;
    state_db
        .metadata()
        .ok()?
        .file_type()
        .is_file()
        .then_some(state_db)
}

#[cfg(not(unix))]
fn open_profile_state_db(
    _profiles_directory: &File,
    _profile_name: &OsStr,
    profile_dir: &Path,
) -> Option<File> {
    let metadata = fs::symlink_metadata(profile_dir).ok()?;
    if !metadata.file_type().is_dir() {
        return None;
    }
    let profile_directory = File::open(profile_dir).ok()?;
    if !profile_directory.metadata().ok()?.file_type().is_dir()
        || !file_matches_path(profile_dir, &profile_directory)
    {
        return None;
    }
    let state_db = open_state_db_file(&profile_dir.join("state.db"), false)?;
    let metadata = fs::symlink_metadata(profile_dir).ok()?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_dir()
        || !file_matches_path(profile_dir, &profile_directory)
    {
        return None;
    }
    Some(state_db)
}

pub(super) fn is_regular_non_symlink(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    fs::symlink_metadata(path)
        .ok()
        .zip(fs::symlink_metadata(parent).ok())
        .map(|(metadata, parent_metadata)| {
            metadata.file_type().is_file() && parent_metadata.file_type().is_dir()
        })
        .unwrap_or(false)
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

pub(super) fn file_matches_path(path: &Path, file: &File) -> bool {
    // A handle comparison detects replacement at the same pathname, which a canonical path string
    // cannot distinguish from the file that was opened during discovery.
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() {
        return false;
    }
    let Ok(path_handle) = same_file::Handle::from_path(path) else {
        return false;
    };
    let Some(file_handle) = file
        .try_clone()
        .ok()
        .and_then(|file| same_file::Handle::from_file(file).ok())
    else {
        return false;
    };
    fs::symlink_metadata(path)
        .map(|metadata| !metadata.file_type().is_symlink() && path_handle == file_handle)
        .unwrap_or(false)
}

#[cfg(unix)]
fn open_path_with_flags(path: &Path, flags: i32) -> Option<File> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let fd = unsafe { libc::open(path.as_ptr(), flags, 0) };
    if fd < 0 {
        return None;
    }
    Some(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

#[cfg(unix)]
fn open_at(parent: &File, name: &OsStr, flags: i32) -> Option<File> {
    let name = CString::new(name.as_bytes()).ok()?;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0) };
    if fd < 0 {
        return None;
    }
    Some(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};

    use super::*;
    use ccusage_test_support::{EnvVarsGuard, Fixture, fs_fixture};

    fn default_home_guard(fixture: &Fixture) -> EnvVarsGuard {
        EnvVarsGuard::set_many([
            ("HOME", Some(fixture.root().as_os_str().to_os_string())),
            (HERMES_HOME_ENV, None),
        ])
    }

    fn discovered_paths() -> Vec<PathBuf> {
        hermes_state_db_paths()
            .unwrap()
            .into_iter()
            .map(|path| path.path)
            .collect()
    }

    #[test]
    fn discovers_default_and_immediate_profile_databases_in_stable_order() {
        let fixture = fs_fixture!({
            ".hermes/state.db": "",
            ".hermes/profiles/personal/state.db": "",
            ".hermes/profiles/work/state.db": "",
            ".hermes/profiles/deep/ignored/state.db": "",
            ".hermes/profiles/not-a-profile": "",
        });
        let _ = fixture.create_dir_all(".hermes/profiles/directory-db/state.db");
        let _env_guard = default_home_guard(&fixture);

        let paths = discovered_paths();

        assert_eq!(
            paths,
            vec![
                fixture.path(".hermes/state.db"),
                fixture.path(".hermes/profiles/personal/state.db"),
                fixture.path(".hermes/profiles/work/state.db"),
            ]
        );
    }

    #[test]
    fn opens_default_and_profile_databases_with_distinct_identities() {
        let fixture = fs_fixture!({
            ".hermes/state.db": "",
            ".hermes/profiles/work/state.db": "",
        });
        let _env_guard = default_home_guard(&fixture);

        let paths = hermes_state_db_paths().unwrap();

        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].profile_name, None);
        assert_eq!(paths[1].profile_name.as_deref(), Some(OsStr::new("work")));
        assert!(!paths[0].no_follow);
        assert!(paths[1].no_follow);
        assert_ne!(paths[0].database_identity, paths[1].database_identity);
    }

    #[test]
    fn discovers_profiles_without_a_default_database() {
        let fixture = fs_fixture!({
            ".hermes/profiles/work/state.db": "",
        });
        let _env_guard = default_home_guard(&fixture);

        let paths = discovered_paths();

        assert_eq!(paths, vec![fixture.path(".hermes/profiles/work/state.db")]);
    }

    #[test]
    fn ignores_missing_profile_directory() {
        let fixture = fs_fixture!({
            ".hermes/state.db": "",
        });
        let _env_guard = default_home_guard(&fixture);

        let paths = discovered_paths();

        assert_eq!(paths, vec![fixture.path(".hermes/state.db")]);
    }

    #[test]
    fn ignores_invalid_profile_directory_and_state_db_entries() {
        let fixture = fs_fixture!({
            ".hermes/state.db": "",
            ".hermes/profiles": "not a directory",
        });
        let _env_guard = default_home_guard(&fixture);

        assert_eq!(discovered_paths(), vec![fixture.path(".hermes/state.db")]);
    }

    #[test]
    fn ignores_non_directory_profiles_and_non_regular_state_dbs() {
        let fixture = fs_fixture!({
            ".hermes/state.db": "",
            ".hermes/profiles/not-a-profile": "file",
        });
        let _ = fixture.create_dir_all(".hermes/profiles/directory-db/state.db");
        let _env_guard = default_home_guard(&fixture);

        assert_eq!(discovered_paths(), vec![fixture.path(".hermes/state.db")]);
    }

    #[test]
    fn explicit_homes_are_authoritative_and_deduplicated() {
        let fixture = fs_fixture!({
            "first/state.db": "",
            "first/profiles/ignored/state.db": "",
            "second/state.db": "",
        });
        let first = fixture.path("first");
        let second = fixture.path("second");
        let homes = [
            first.display().to_string(),
            format!("{}/./", first.display()),
            second.display().to_string(),
            fixture.path("missing").display().to_string(),
        ]
        .join(",");
        let _env_guard = EnvVarsGuard::set_many([
            ("HOME", Some(fixture.root().as_os_str().to_os_string())),
            (HERMES_HOME_ENV, Some(OsString::from(homes))),
        ]);

        let paths = discovered_paths();

        assert_eq!(
            paths,
            vec![
                fixture.path("first/state.db"),
                fixture.path("second/state.db")
            ]
        );
    }

    #[test]
    fn empty_explicit_home_disables_default_discovery() {
        let fixture = fs_fixture!({
            ".hermes/state.db": "",
            ".hermes/profiles/work/state.db": "",
        });
        let _env_guard = EnvVarsGuard::set_many([
            ("HOME", Some(fixture.root().as_os_str().to_os_string())),
            (HERMES_HOME_ENV, Some(OsString::new())),
        ]);

        assert!(discovered_paths().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn follows_default_root_state_db_symlinks() {
        use std::os::unix::fs::symlink;

        let fixture = fs_fixture!({
            ".hermes/state-target.db": "",
        });
        symlink(
            fixture.path(".hermes/state-target.db"),
            fixture.path(".hermes/state.db"),
        )
        .unwrap();
        let _env_guard = default_home_guard(&fixture);

        assert_eq!(discovered_paths(), vec![fixture.path(".hermes/state.db")]);
    }

    #[cfg(unix)]
    #[test]
    fn preserves_explicit_state_db_symlink_compatibility() {
        use std::os::unix::fs::symlink;

        let fixture = fs_fixture!({
            "state-target.db": "",
        });
        symlink(fixture.path("state-target.db"), fixture.path("state.db")).unwrap();
        let _env_guard = EnvVarsGuard::set_many([(
            HERMES_HOME_ENV,
            Some(fixture.root().as_os_str().to_os_string()),
        )]);

        assert_eq!(discovered_paths(), vec![fixture.path("state.db")]);
    }

    #[cfg(unix)]
    #[test]
    fn canonicalizes_duplicate_explicit_roots() {
        use std::os::unix::fs::symlink;

        let fixture = fs_fixture!({
            "first/state.db": "",
        });
        let alias = fixture.path("alias");
        symlink(fixture.path("first"), &alias).unwrap();
        let homes = [fixture.path("first"), alias]
            .map(|path| path.display().to_string())
            .join(",");
        let _env_guard = EnvVarsGuard::set_many([(HERMES_HOME_ENV, Some(OsString::from(homes)))]);

        let paths = discovered_paths();

        assert_eq!(paths, vec![fixture.path("first/state.db")]);
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_profile_directory_or_state_db_symlinks() {
        use std::os::unix::fs::symlink;

        let fixture = fs_fixture!({
            ".hermes/state.db": "",
            ".hermes/profiles/real/state.db": "",
        });
        symlink(
            fixture.path(".hermes/profiles/real"),
            fixture.path(".hermes/profiles/linked"),
        )
        .unwrap();
        let symlinked_state_dir = fixture.create_dir_all(".hermes/profiles/symlinked-state");
        symlink(
            fixture.path(".hermes/state.db"),
            symlinked_state_dir.join("state.db"),
        )
        .unwrap();
        let _env_guard = default_home_guard(&fixture);

        let paths = discovered_paths();

        assert_eq!(
            paths,
            vec![
                fixture.path(".hermes/state.db"),
                fixture.path(".hermes/profiles/real/state.db"),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn does_not_follow_profile_state_db_symlinks() {
        use std::os::windows::fs::symlink_file;

        let fixture = fs_fixture!({
            ".hermes/state.db": "",
            ".hermes/profiles/real/state.db": "",
        });
        let symlinked_state_dir = fixture.create_dir_all(".hermes/profiles/symlinked-state");
        symlink_file(
            fixture.path(".hermes/profiles/real/state.db"),
            symlinked_state_dir.join("state.db"),
        )
        .unwrap();
        let _env_guard = default_home_guard(&fixture);

        assert_eq!(
            discovered_paths(),
            vec![
                fixture.path(".hermes/state.db"),
                fixture.path(".hermes/profiles/real/state.db"),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn derives_the_same_identity_from_opened_hard_links() {
        let fixture = fs_fixture!({ "original.db": "" });
        let original = fixture.path("original.db");
        let alias = fixture.path("alias.db");
        fs::hard_link(&original, &alias).unwrap();

        let original_identity = DatabaseIdentity::from_open_file(&File::open(original).unwrap());
        let alias_identity = DatabaseIdentity::from_open_file(&File::open(alias).unwrap());

        assert!(original_identity.is_some());
        assert_eq!(original_identity, alias_identity);
    }
}

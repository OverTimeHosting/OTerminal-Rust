//! Locating the git executable when it is not on this process's `PATH`.
//!
//! OTerminal can be started with a `PATH` that lacks Git even though Git is
//! installed, e.g. when it is launched by another tool that passes a reduced
//! environment. Without a git binary every repository fails to open ("no git
//! binary available"): the git panel shows no changes and no history, and the
//! branch and worktree pickers stay empty. On Windows, [`ensure_git_on_path`]
//! finds Git for Windows through the persisted `PATH` in the registry, the Git
//! for Windows install location and the usual install folders, and puts it on
//! this process's `PATH` so that repositories, `git clone` and terminals work.

use std::path::{Path, PathBuf};

/// The file name of the git executable on this platform.
const GIT_EXECUTABLE: &str = if cfg!(windows) { "git.exe" } else { "git" };

/// Expands `%NAME%` references using `lookup`. Unknown variables and a lone
/// `%` are kept as they are, like `ExpandEnvironmentStrings` does.
pub fn expand_windows_env_vars(value: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut result = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        result.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => {
                let name = &after[..end];
                match lookup(name) {
                    Some(expanded) => result.push_str(&expanded),
                    None => {
                        result.push('%');
                        result.push_str(name);
                        result.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                result.push('%');
                rest = after;
            }
        }
    }
    result.push_str(rest);
    result
}

/// The directories of a `;`-separated `PATH` value, with `%NAME%` references
/// expanded and surrounding quotes removed. Empty and relative entries are
/// skipped.
pub fn windows_path_entries(
    path_value: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<PathBuf> {
    path_value
        .split(';')
        .map(|entry| expand_windows_env_vars(entry.trim(), &lookup))
        .map(|entry| entry.trim().trim_matches('"').to_string())
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .filter(|entry| entry.is_absolute())
        .collect()
}

/// Folders where Git for Windows (and common alternatives) install
/// `git.exe`, relative to the given base folders, in order of preference.
pub fn well_known_windows_git_locations(lookup: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let mut locations = Vec::new();
    for program_files in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = lookup(program_files) {
            locations.push(
                Path::new(&base)
                    .join("Git")
                    .join("cmd")
                    .join(GIT_EXECUTABLE),
            );
        }
    }
    if let Some(local_app_data) = lookup("LOCALAPPDATA") {
        locations.push(
            Path::new(&local_app_data)
                .join("Programs")
                .join("Git")
                .join("cmd")
                .join(GIT_EXECUTABLE),
        );
    }
    if let Some(user_profile) = lookup("USERPROFILE") {
        let scoop = Path::new(&user_profile).join("scoop");
        locations.push(
            scoop
                .join("apps")
                .join("git")
                .join("current")
                .join("cmd")
                .join(GIT_EXECUTABLE),
        );
        locations.push(scoop.join("shims").join(GIT_EXECUTABLE));
    }
    locations
}

/// Returns the first `git` executable found in `directories`.
pub fn find_git_in_directories(directories: &[PathBuf]) -> Option<PathBuf> {
    directories
        .iter()
        .map(|directory| directory.join(GIT_EXECUTABLE))
        .find(|candidate| candidate.is_file())
}

fn process_env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Finds a git executable that is installed but not on this process's `PATH`.
///
/// Only Windows is searched; elsewhere this returns `None`.
pub fn find_git_outside_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        windows::find_git()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Makes sure `git` can be found through this process's `PATH`.
///
/// Returns `Ok(None)` when git already was on `PATH` (or none could be found
/// anywhere), and `Ok(Some(git))` when a git executable found elsewhere had
/// its folder prepended to `PATH`.
///
/// # Safety
///
/// This modifies the process environment, so it must be called while the
/// process is still single-threaded (at the start of `main`).
pub unsafe fn ensure_git_on_path() -> Option<PathBuf> {
    if !cfg!(windows) || git_on_process_path() {
        return None;
    }
    let git = find_git_outside_path()?;
    let git_directory = git.parent()?.to_path_buf();
    let mut directories = vec![git_directory];
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path));
    }
    let path = std::env::join_paths(directories).ok()?;
    // SAFETY: the caller guarantees that no other thread is running.
    unsafe { std::env::set_var("PATH", path) };
    Some(git)
}

fn git_on_process_path() -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        let directories = std::env::split_paths(&path).collect::<Vec<_>>();
        find_git_in_directories(&directories).is_some()
    })
}

#[cfg(windows)]
mod windows {
    use super::*;

    const MACHINE_ENVIRONMENT_KEY: &str =
        "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment";
    const USER_ENVIRONMENT_KEY: &str = "Environment";
    const GIT_FOR_WINDOWS_KEY: &str = "SOFTWARE\\GitForWindows";

    fn registry_string(root: &windows_registry::Key, key: &str, name: &str) -> Option<String> {
        let value = root.open(key).ok()?.get_hstring(name).ok()?;
        Some(value.to_string_lossy()).filter(|value| !value.is_empty())
    }

    /// The machine and user `PATH` as stored in the registry, i.e. the `PATH`
    /// a freshly started program gets from Explorer.
    fn persisted_path_entries() -> Vec<PathBuf> {
        let mut entries = Vec::new();
        for (root, key) in [
            (windows_registry::LOCAL_MACHINE, MACHINE_ENVIRONMENT_KEY),
            (windows_registry::CURRENT_USER, USER_ENVIRONMENT_KEY),
        ] {
            if let Some(path) = registry_string(root, key, "Path") {
                entries.extend(windows_path_entries(&path, process_env_lookup));
            }
        }
        entries
    }

    fn git_for_windows_install_paths() -> Vec<PathBuf> {
        [
            windows_registry::LOCAL_MACHINE,
            windows_registry::CURRENT_USER,
        ]
        .into_iter()
        .filter_map(|root| registry_string(root, GIT_FOR_WINDOWS_KEY, "InstallPath"))
        .map(|install_path| Path::new(&install_path).join("cmd").join(GIT_EXECUTABLE))
        .collect()
    }

    pub(super) fn find_git() -> Option<PathBuf> {
        find_git_in_directories(&persisted_path_entries()).or_else(|| {
            git_for_windows_install_paths()
                .into_iter()
                .chain(well_known_windows_git_locations(process_env_lookup))
                .find(|candidate| candidate.is_file())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(name: &str) -> Option<String> {
        match name {
            "SystemRoot" => Some(r"C:\WINDOWS".to_string()),
            "ProgramFiles" => Some(r"C:\Program Files".to_string()),
            "LOCALAPPDATA" => Some(r"C:\Users\me\AppData\Local".to_string()),
            _ => None,
        }
    }

    #[test]
    fn expands_known_variables_and_keeps_unknown_ones() {
        assert_eq!(
            expand_windows_env_vars(r"%SystemRoot%\system32", lookup),
            r"C:\WINDOWS\system32"
        );
        assert_eq!(
            expand_windows_env_vars(r"%NOPE%\bin;100%", lookup),
            r"%NOPE%\bin;100%"
        );
        assert_eq!(expand_windows_env_vars("%%", lookup), "%%");
        assert_eq!(expand_windows_env_vars("plain", lookup), "plain");
    }

    #[cfg(windows)]
    #[test]
    fn parses_registry_path_values() {
        let entries = windows_path_entries(
            r#"%SystemRoot%\system32;;"%ProgramFiles%\Git\cmd" ; relative\dir;C:\Tools\"#,
            lookup,
        );
        assert_eq!(
            entries,
            vec![
                PathBuf::from(r"C:\WINDOWS\system32"),
                PathBuf::from(r"C:\Program Files\Git\cmd"),
                PathBuf::from(r"C:\Tools\"),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn well_known_locations_prefer_program_files() {
        let locations = well_known_windows_git_locations(lookup);
        assert_eq!(
            locations.first(),
            Some(&PathBuf::from(r"C:\Program Files\Git\cmd\git.exe"))
        );
        assert!(locations.contains(&PathBuf::from(
            r"C:\Users\me\AppData\Local\Programs\Git\cmd\git.exe"
        )));
    }

    #[test]
    fn finds_git_in_directories() {
        let temp = tempfile::tempdir().unwrap();
        let empty = temp.path().join("empty");
        let with_git = temp.path().join("git").join("cmd");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&with_git).unwrap();
        std::fs::write(with_git.join(GIT_EXECUTABLE), b"").unwrap();

        assert_eq!(
            find_git_in_directories(&[empty.clone(), with_git.clone()]),
            Some(with_git.join(GIT_EXECUTABLE))
        );
        assert_eq!(find_git_in_directories(&[empty]), None);
    }
}

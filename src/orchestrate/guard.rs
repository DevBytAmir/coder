use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use crate::orchestrate::project_root::find_git_root;

/// Directories that are never safe to sandbox. The sentinel `"home"` means the caller's
/// actual home directory, every other entry is an absolute path. All but `"home"`, `"/"`,
/// and `"/home"` also refuse every descendant (e.g. `/etc/ssh`), not just an exact match:
/// `home` and the `/home` mount point are left exact-only since nearly all real projects
/// live under a home subdirectory, and `/` is left exact-only because every absolute path
/// is technically its descendant.
const REFUSE_LIST: &[&str] = &[
    "home", "/", "/home", "/root", "/etc", "/usr", "/var", "/boot",
];

const EXACT_MATCH_ONLY: &[&str] = &["home", "/", "/home"];

/// Resolve a path to a canonical, comparable form without requiring it to
/// exist on disk: canonicalize if possible (resolves symlinks too), else
/// fall back to a lexical normalization of `.`/`..` components so a
/// not-yet-existing path like `/home/amir/../../etc` still compares equal
/// to `/etc`.
fn resolve_lexical(path: &Path) -> PathBuf {
    if let Ok(canon) = path.canonicalize() {
        return canon;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// True if `project_dir` is, or is a descendant of (except the `home`/`/`/`/home`
/// exact-only entries, see [`REFUSE_LIST`]), one of the never-safe-to-sandbox directories.
fn is_refused(canon_project: &Path, home: &Path) -> bool {
    REFUSE_LIST.iter().any(|entry| {
        let candidate = if *entry == "home" {
            home.to_path_buf()
        } else {
            PathBuf::from(entry)
        };
        let canon_candidate = resolve_lexical(&candidate);
        let exact_match_only = EXACT_MATCH_ONLY.contains(entry);
        canon_project == canon_candidate
            || (!exact_match_only && canon_project.starts_with(&canon_candidate))
    })
}

/// Refuse to sandbox dangerous directories; otherwise prompt for confirmation.
/// Returns `Ok(false)` on refusal or decline, `Ok(true)` on explicit confirmation.
///
/// # Errors
///
/// Returns an error if reading the confirmation prompt from stdin fails.
pub fn guard_confirm(project_dir: &Path, home: &Path) -> anyhow::Result<bool> {
    let canon_project = resolve_lexical(project_dir);

    if is_refused(&canon_project, home) {
        return Ok(false);
    }

    println!("Project folder: {}", canon_project.display());
    if let Some(root) = find_git_root(&canon_project) {
        println!("Git repo root: {}", root.display());
    }
    print!("Proceed? [y/N] ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    Ok(matches!(input, "y" | "Y" | "yes" | "YES"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_refuses_home_directory() {
        let home = Path::new("/home/amir");
        assert!(guard_confirm(home, home).is_ok_and(|ok| !ok));
    }

    #[test]
    fn guard_refuses_root() {
        assert!(guard_confirm(Path::new("/"), Path::new("/home/amir")).is_ok_and(|ok| !ok));
    }

    #[test]
    fn guard_refuses_nonexistent_path_that_lexically_resolves_to_refused_dir() {
        let path = Path::new("/home/amir/../../etc/nonexistent-marker-dir/../");
        assert!(guard_confirm(path, Path::new("/home/amir")).is_ok_and(|ok| !ok));
    }

    #[test]
    fn ordinary_project_under_home_is_not_refused() {
        // Regression: REFUSE_LIST has both the "home" sentinel AND a literal "/home"
        // mount-point entry. Subpath containment must exempt both, or every real
        // project (which nearly always lives under /home/<user>/...) gets refused.
        let project = Path::new("/home/amir/Projects/test");
        assert!(!is_refused(project, Path::new("/home/amir")));
    }

    #[test]
    fn guard_refuses_a_subdirectory_of_a_refused_system_path() {
        let path = Path::new("/etc/ssh");
        assert!(guard_confirm(path, Path::new("/home/amir")).is_ok_and(|ok| !ok));
    }
}

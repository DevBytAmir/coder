use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use crate::exit::{self, ExitError};
use crate::orchestrate::project_root::find_git_root;
use crate::paths;

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
/// not-yet-existing path like `/home/user/../../etc` still compares equal
/// to `/etc`.
#[must_use]
pub fn resolve_lexical(path: &Path) -> PathBuf {
    if let Ok(canon) = paths::canonicalize(path) {
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
    is_refused_windows_location(canon_project) || in_refuse_list(canon_project, home)
}

/// Windows folders never safe to sandbox, natively or under `/mnt/<drive>`: drive roots,
/// system folders and everything in them, and `Users` and each profile (exact only).
/// Case-insensitive.
fn is_refused_windows_location(canon_project: &Path) -> bool {
    const SYSTEM_DIRS: &[&str] = &[
        "windows",
        "program files",
        "program files (x86)",
        "programdata",
    ];
    let text = canon_project
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let parts: Vec<&str> = text.split('/').filter(|p| !p.is_empty()).collect();
    let rest = match parts.as_slice() {
        // native: "c:", "c:/windows", ...
        [drive, rest @ ..]
            if drive.len() == 2
                && drive.ends_with(':')
                && drive.as_bytes()[0].is_ascii_alphabetic() =>
        {
            rest
        }
        // WSL automount: "/mnt/c", "/mnt/c/windows", ...
        ["mnt", drive, rest @ ..]
            if drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic() =>
        {
            rest
        }
        _ => return false,
    };
    match rest {
        [] | ["users"] | ["users", _] => true,
        [first, ..] => SYSTEM_DIRS.contains(first),
    }
}

fn in_refuse_list(canon_project: &Path, home: &Path) -> bool {
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

/// [`guard_confirm`]'s refusal without the prompt, so `--dry-run` can refuse too.
#[must_use]
pub fn is_refused_dir(project_dir: &Path, home: &Path) -> bool {
    is_refused(&resolve_lexical(project_dir), home)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    Prompt,
    /// `--yes`: proceed without asking. Never overrides a refusal.
    AssumeYes,
}

/// Refuse to sandbox dangerous directories; otherwise confirm, either by prompting or,
/// with [`Confirmation::AssumeYes`], without asking. Returns `Ok(false)` on refusal or
/// decline, `Ok(true)` on confirmation.
///
/// `interactive` is whether stdin is a terminal; without one a prompt would read EOF and
/// look like a decline, so that is an explicit error instead. The prompt goes to stderr
/// so it stays visible when stdout is piped.
///
/// # Errors
///
/// Returns an [`ExitError`] with [`exit::CONFIRMATION_REQUIRED`] when a prompt is needed
/// but `interactive` is false, or an I/O error if reading the prompt from stdin fails.
pub fn guard_confirm(
    project_dir: &Path,
    home: &Path,
    confirmation: Confirmation,
    interactive: bool,
) -> anyhow::Result<bool> {
    let canon_project = resolve_lexical(project_dir);

    if is_refused(&canon_project, home) {
        return Ok(false);
    }

    if confirmation == Confirmation::AssumeYes {
        eprintln!("sandboxing {} (--yes)", canon_project.display());
        return Ok(true);
    }

    if !interactive {
        return Err(ExitError::new(
            exit::CONFIRMATION_REQUIRED,
            format!(
                "confirmation required to sandbox {}, but stdin is not a terminal; \
                 re-run with `--yes -C <dir>` to proceed without prompting",
                canon_project.display()
            ),
        )
        .into());
    }

    eprintln!("Project folder: {}", canon_project.display());
    if let Some(root) = find_git_root(&canon_project) {
        eprintln!("Git repo root: {}", root.display());
    }
    eprint!("Proceed? [y/N] ");
    io::stderr().flush()?;

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
        let home = Path::new("/home/user");
        assert!(guard_confirm(home, home, Confirmation::Prompt, true).is_ok_and(|ok| !ok));
    }

    #[test]
    fn guard_refuses_windows_locations_native_and_wsl() {
        for refused in [
            r"C:\",
            r"E:\",
            r"C:\Windows",
            r"c:\windows\System32",
            r"C:\Program Files",
            r"C:\Program Files (x86)\Foo",
            r"C:\ProgramData",
            r"C:\Users",
            r"C:\Users\user",
            r"C:\USERS\User",
            "/mnt/c",
            "/mnt/e/",
            "/mnt/c/Windows",
            "/mnt/c/windows/system32",
            "/mnt/c/Program Files",
            "/mnt/c/Users",
            "/mnt/c/Users/User",
        ] {
            assert!(
                is_refused_windows_location(Path::new(refused)),
                "{refused} should be refused"
            );
        }
        for allowed in [
            r"C:\Users\user\project",
            r"E:\work\app",
            r"C:\src\app",
            "/mnt/c/Users/User/project",
            "/mnt/e/work/app",
            "/mnt/storage",
            "/mnt/storage/Windows",
            "/home/user/project",
        ] {
            assert!(
                !is_refused_windows_location(Path::new(allowed)),
                "{allowed} should be allowed"
            );
        }
    }

    #[test]
    fn dry_run_refusal_matches_guard() {
        let home = Path::new("/home/user");
        assert!(is_refused_dir(home, home));
        assert!(is_refused_dir(Path::new("/etc"), home));
        assert!(is_refused_dir(Path::new("/mnt/c"), home));
        assert!(!is_refused_dir(Path::new("/home/user/project"), home));
    }

    #[test]
    fn guard_refuses_root() {
        assert!(guard_confirm(
            Path::new("/"),
            Path::new("/home/user"),
            Confirmation::Prompt,
            true
        )
        .is_ok_and(|ok| !ok));
    }

    #[test]
    fn guard_refuses_nonexistent_path_that_lexically_resolves_to_refused_dir() {
        let path = Path::new("/home/user/../../etc/nonexistent-marker-dir/../");
        assert!(
            guard_confirm(path, Path::new("/home/user"), Confirmation::Prompt, true)
                .is_ok_and(|ok| !ok)
        );
    }

    #[test]
    fn ordinary_project_under_home_is_not_refused() {
        // Regression: REFUSE_LIST has both the "home" sentinel AND a literal "/home"
        // mount-point entry. Subpath containment must exempt both, or every real
        // project (which nearly always lives under /home/<user>/...) gets refused.
        let project = Path::new("/home/user/projects/test");
        assert!(!is_refused(project, Path::new("/home/user")));
    }

    #[test]
    fn guard_refuses_a_subdirectory_of_a_refused_system_path() {
        let path = Path::new("/etc/ssh");
        assert!(
            guard_confirm(path, Path::new("/home/user"), Confirmation::Prompt, true)
                .is_ok_and(|ok| !ok)
        );
    }

    #[test]
    fn assume_yes_never_overrides_a_refusal() {
        let home = Path::new("/home/user");
        for dir in [home, Path::new("/"), Path::new("/etc/ssh")] {
            assert!(
                guard_confirm(dir, home, Confirmation::AssumeYes, false).is_ok_and(|ok| !ok),
                "--yes must not sandbox {}",
                dir.display()
            );
        }
    }

    /// A project directory the guard accepts. Not the system temp dir: on macOS that
    /// lives under `/var`, which the guard refuses.
    fn ordinary_project() -> tempfile::TempDir {
        tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap()
    }

    #[test]
    fn assume_yes_confirms_an_ordinary_project_without_a_terminal() {
        let project = ordinary_project();
        let ok = guard_confirm(
            project.path(),
            Path::new("/home/user"),
            Confirmation::AssumeYes,
            false,
        )
        .unwrap();
        assert!(ok);
    }

    #[test]
    fn prompting_without_a_terminal_is_an_explicit_error_not_a_decline() {
        let project = ordinary_project();
        let err = guard_confirm(
            project.path(),
            Path::new("/home/user"),
            Confirmation::Prompt,
            false,
        )
        .unwrap_err();
        assert_eq!(exit::code_for(&err), exit::CONFIRMATION_REQUIRED);
        assert!(err.to_string().contains("--yes"), "{err}");
    }
}

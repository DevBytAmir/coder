use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::tools::ProjectRoot;

/// The tool's first extra arg as an existing path, resolved against `effective_cwd` (the
/// `-C` directory), never the process cwd.
#[must_use]
pub fn existing_file_arg(effective_cwd: &Path, arg: Option<&str>) -> Option<PathBuf> {
    arg.map(|a| effective_cwd.join(a)).filter(|p| p.exists())
}

/// The git repository root containing `dir`, if any. Shared by `resolve_project_dir`'s
/// `GitRootOfArg` strategy and the sandbox-guard's confirmation-prompt display, so both
/// report the same root rather than two independent `git rev-parse` calls drifting apart.
#[must_use]
pub fn find_git_root(dir: &Path) -> Option<PathBuf> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| PathBuf::from(s.trim()))
}

/// Resolve the project directory per `strategy`, and rewrite any extra args
/// (today just the single `arg`, if given) relative to the resolved root.
///
/// # Errors
///
/// Returns an error if `arg` exists but can't be canonicalized.
pub fn resolve_project_dir(
    strategy: ProjectRoot,
    cwd: &Path,
    arg: Option<&Path>,
) -> anyhow::Result<(PathBuf, Vec<String>)> {
    match strategy {
        ProjectRoot::Cwd => Ok((cwd.to_path_buf(), Vec::new())),
        ProjectRoot::GitRootOfArg => {
            let arg = match arg {
                Some(a) if a.exists() => a,
                _ => return Ok((cwd.to_path_buf(), Vec::new())),
            };
            let canon = arg.canonicalize()?;
            let dirname = canon.parent().unwrap_or(&canon);

            let root = find_git_root(dirname).unwrap_or_else(|| dirname.to_path_buf());
            // canon has a \\?\ prefix on Windows (from canonicalize); root may not,
            // depending on its source, so canonicalize it too before strip_prefix.
            let root = root.canonicalize().unwrap_or(root);

            // Always a path inside a Linux container, so forward slashes regardless
            // of the host's own path separator.
            let rewritten = canon.strip_prefix(&root).map_or_else(
                |_| canon.to_string_lossy().replace('\\', "/"),
                |p| p.to_string_lossy().replace('\\', "/"),
            );

            Ok((root, vec![rewritten]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_arg_resolves_against_effective_cwd_not_process_cwd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("only-here.txt"), "x").unwrap();
        assert_eq!(
            existing_file_arg(dir.path(), Some("only-here.txt")),
            Some(dir.path().join("only-here.txt"))
        );
        assert_eq!(existing_file_arg(dir.path(), Some("--help")), None);
        assert_eq!(existing_file_arg(dir.path(), None), None);
    }

    #[test]
    fn cwd_strategy_ignores_arg() {
        let (dir, _) =
            resolve_project_dir(ProjectRoot::Cwd, Path::new("/home/amir/proj"), None).unwrap();
        assert_eq!(dir, Path::new("/home/amir/proj"));
    }

    #[test]
    fn git_root_strategy_finds_repo_root_for_file_arg() {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .arg("init")
            .current_dir(dir.path())
            .status()
            .unwrap();
        let file = dir.path().join("sub/file.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "x").unwrap();
        let (root, args) =
            resolve_project_dir(ProjectRoot::GitRootOfArg, dir.path(), Some(&file)).unwrap();
        // `root` comes from `git`'s own output, which (unlike Rust's canonicalize on
        // Windows) doesn't add a `\\?\` verbatim prefix. Canonicalize both sides so the
        // comparison doesn't depend on which form either one happens to produce.
        assert_eq!(
            root.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
        assert_eq!(args, vec!["sub/file.txt"]);
    }

    #[test]
    fn git_root_strategy_falls_back_to_cwd_when_no_arg() {
        let (dir, args) = resolve_project_dir(
            ProjectRoot::GitRootOfArg,
            Path::new("/home/amir/proj"),
            None,
        )
        .unwrap();
        assert_eq!(dir, Path::new("/home/amir/proj"));
        assert_eq!(args, Vec::<String>::new());
    }

    #[test]
    fn git_root_strategy_falls_back_to_cwd_when_arg_does_not_exist() {
        let (dir, args) = resolve_project_dir(
            ProjectRoot::GitRootOfArg,
            Path::new("/home/amir/proj"),
            Some(Path::new("--help")),
        )
        .unwrap();
        assert_eq!(dir, Path::new("/home/amir/proj"));
        assert_eq!(args, Vec::<String>::new());
    }
}

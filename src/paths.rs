//! Path helpers that behave the same on Unix and Windows.

use std::io;
use std::path::{Path, PathBuf};

/// `std::fs::canonicalize` without Windows' `\\?\` prefix on drive paths, which
/// podman's `-v` can't take. Verbatim UNC paths are left as they are, so callers can
/// refuse them.
///
/// # Errors
///
/// Returns `canonicalize`'s error, e.g. when the path doesn't exist.
pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    Ok(strip_verbatim(path.canonicalize()?))
}

/// Strip `\\?\` from a verbatim drive path; leave everything else as is.
#[must_use]
pub fn strip_verbatim(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    match text.strip_prefix(r"\\?\") {
        Some(rest) if is_drive_path(rest) => PathBuf::from(rest),
        _ => path,
    }
}

/// `C:\…` or `C:/…` (or a bare `C:\`).
fn is_drive_path(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// A UNC path (`\\server\share\…`, `\\wsl$\…`, or verbatim `\\?\UNC\…`).
#[must_use]
pub fn is_unc(path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    // `//server/share` is UNC only on Windows; on Unix it's just `/server/share`
    let unc = text.starts_with(r"\\") || (cfg!(windows) && text.starts_with("//"));
    unc && !text.strip_prefix(r"\\?\").is_some_and(is_drive_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_verbatim_drive_prefix_only() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\Users\a\project")),
            PathBuf::from(r"C:\Users\a\project")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\E:\")),
            PathBuf::from(r"E:\")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\wsl$\Debian\home")),
            PathBuf::from(r"\\?\UNC\wsl$\Debian\home")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from("/home/a")),
            PathBuf::from("/home/a")
        );
    }

    #[test]
    fn detects_unc() {
        assert!(is_unc(Path::new(r"\\wsl$\Debian\home\a")));
        assert!(is_unc(Path::new(r"\\?\UNC\server\share")));
        assert!(is_unc(Path::new(r"\\server\share")));
        assert!(!is_unc(Path::new(r"\\?\C:\Users")));
        assert!(!is_unc(Path::new(r"C:\Users")));
        assert!(!is_unc(Path::new("/home/a")));
        assert_eq!(is_unc(Path::new("//server/share")), cfg!(windows));
    }

    #[cfg(windows)]
    #[test]
    fn canonicalize_has_no_verbatim_prefix_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let canon = canonicalize(dir.path()).unwrap();
        let text = canon.to_str().unwrap();
        assert!(!text.starts_with(r"\\?\"), "{text}");
        assert!(canon.is_absolute());
    }
}

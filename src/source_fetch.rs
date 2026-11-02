use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use tar::Archive;

/// GitHub's tag-tarball URL for `version` (no `v` prefix): no `git` binary required,
/// just an HTTP GET, unlike `git clone`.
#[must_use]
pub fn source_tarball_url(repo: &str, version: &str) -> String {
    format!("https://codeload.github.com/{repo}/tar.gz/refs/tags/v{version}")
}

/// GitHub's tarball extracts to a single top-level directory named `<repo>-<tag>`
/// (slashes replaced). Find it, since the exact name isn't worth hardcoding.
///
/// # Errors
///
/// Returns an error if `dir` doesn't contain exactly one subdirectory.
pub fn find_single_subdir(dir: &Path) -> Result<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    match entries.len() {
        1 => Ok(entries.remove(0)),
        n => bail!(
            "expected exactly one extracted subdirectory in {}, found {n}",
            dir.display()
        ),
    }
}

/// True if `dir` already looks like an extracted `coder` checkout, for `--keep`'s
/// reuse-without-redownloading check.
#[must_use]
pub fn looks_like_source_checkout(dir: &Path) -> bool {
    dir.join("Cargo.toml").is_file() && dir.join("containers").is_dir()
}

/// `fetch_source` extracts into a subdirectory of `dest` (GitHub's tarball layout),
/// so a `--keep <dest>` reuse check has to look one level deeper than `dest` itself.
/// Returns that subdirectory if `dest` exists and it looks like a real checkout.
#[must_use]
pub fn find_existing_checkout(dest: &Path) -> Option<PathBuf> {
    let root = find_single_subdir(dest).ok()?;
    looks_like_source_checkout(&root).then_some(root)
}

/// Downloads and extracts the tagged source tarball for `version` into `dest`, and
/// returns the path to the extracted source root (a subdirectory of `dest`).
///
/// # Errors
///
/// Returns an error if the download, decompression, or extraction fails.
pub fn fetch_source(repo: &str, version: &str, dest: &Path) -> Result<PathBuf> {
    let url = source_tarball_url(repo, version);
    let resp = reqwest::blocking::get(&url)
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    let bytes = resp.bytes().context("reading tarball response body")?;
    Archive::new(GzDecoder::new(&bytes[..]))
        .unpack(dest)
        .with_context(|| format!("extracting tarball into {}", dest.display()))?;
    find_single_subdir(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_tarball_url_points_at_codeload_tag() {
        let url = source_tarball_url("DevBytAmir/coder", "0.1.0");
        assert_eq!(
            url,
            "https://codeload.github.com/DevBytAmir/coder/tar.gz/refs/tags/v0.1.0"
        );
    }

    #[test]
    fn find_single_subdir_locates_the_one_extracted_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("coder-0.1.0");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(find_single_subdir(dir.path()).unwrap(), sub);
    }

    #[test]
    fn find_single_subdir_ignores_sibling_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("archive.tar.gz"), b"").unwrap();
        let sub = dir.path().join("coder-0.1.0");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(find_single_subdir(dir.path()).unwrap(), sub);
    }

    #[test]
    fn find_single_subdir_rejects_zero_or_multiple() {
        let empty = tempfile::tempdir().unwrap();
        assert!(find_single_subdir(empty.path()).is_err());

        let two = tempfile::tempdir().unwrap();
        std::fs::create_dir(two.path().join("a")).unwrap();
        std::fs::create_dir(two.path().join("b")).unwrap();
        assert!(find_single_subdir(two.path()).is_err());
    }

    #[test]
    fn find_existing_checkout_looks_one_level_inside_dest() {
        // Regression: fetch_source extracts into a subdirectory of `dest` (GitHub's
        // tag-tarball layout), so checking `dest` itself for Cargo.toml/containers
        // would always miss a real, already-fetched checkout and redownload it
        // every time instead of reusing it.
        let dest = tempfile::tempdir().unwrap();
        assert!(
            find_existing_checkout(dest.path()).is_none(),
            "nothing extracted yet"
        );

        let sub = dest.path().join("coder-0.1.0");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("Cargo.toml"), b"").unwrap();
        std::fs::create_dir(sub.join("containers")).unwrap();
        assert_eq!(find_existing_checkout(dest.path()), Some(sub));
    }

    #[test]
    fn looks_like_source_checkout_requires_cargo_toml_and_containers_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!looks_like_source_checkout(dir.path()));
        std::fs::write(dir.path().join("Cargo.toml"), b"").unwrap();
        assert!(
            !looks_like_source_checkout(dir.path()),
            "containers/ missing"
        );
        std::fs::create_dir(dir.path().join("containers")).unwrap();
        assert!(looks_like_source_checkout(dir.path()));
    }
}

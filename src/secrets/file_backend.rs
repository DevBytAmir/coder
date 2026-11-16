use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::Context;

use crate::secrets::SecretsSource;

fn strip_matching_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// A Windows drive under WSL (`/mnt/c/...`), where `chmod` doesn't stick.
#[cfg(unix)]
fn on_wsl_windows_drive(path: &std::path::Path) -> bool {
    let in_wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists();
    in_wsl && is_windows_drive_mount(path)
}

/// `/mnt/<letter>/...`, where WSL mounts Windows drives.
#[cfg(unix)]
fn is_windows_drive_mount(path: &std::path::Path) -> bool {
    let mut parts = path.components().map(|c| c.as_os_str().to_string_lossy());
    parts.next().as_deref() == Some("/")
        && parts.next().as_deref() == Some("mnt")
        && parts
            .next()
            .is_some_and(|d| d.len() == 1 && d.as_bytes()[0].is_ascii_alphabetic())
}

/// Reads provider API keys from a plaintext `KEY=value` file (e.g. `ANTHROPIC_API_KEY=abc`).
/// Refuses to read the file at all unless its permissions are exactly owner read/write (0600).
pub struct FileSource {
    pub path: PathBuf,
}

impl SecretsSource for FileSource {
    fn load(&self, provider_names: &[String]) -> anyhow::Result<HashMap<String, String>> {
        // Check permissions on the open fd, not the path, to avoid a swap/chmod race.
        let mut file = File::open(&self.path)
            .with_context(|| format!("opening secrets file {}", self.path.display()))?;
        #[cfg(unix)]
        {
            let mode = file.metadata()?.permissions().mode();
            if mode & 0o077 != 0 {
                if on_wsl_windows_drive(&self.path) {
                    anyhow::bail!(
                        "secrets file {} has loose permissions, and `chmod` can't fix that on a \
                         Windows drive mounted in WSL; keep the file in the Linux filesystem \
                         (e.g. ~/.config/coder/secrets.env, mode 600), or enable `metadata` for \
                         the drive in /etc/wsl.conf",
                        self.path.display()
                    );
                }
                anyhow::bail!(
                    "secrets file {} has loose permissions; run `chmod 600 {}`",
                    self.path.display(),
                    self.path.display()
                );
            }
        }
        // Windows has no POSIX mode bits; ACLs are the real equivalent but checking
        // them portably needs another crate, so this falls back to trusting the file
        // as-is there. Keep it out of version control and readable by your user only.

        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let mut parsed: HashMap<String, String> = HashMap::new();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let value = strip_matching_quotes(value.trim());
                if !value.is_empty() {
                    parsed.insert(key.trim().to_string(), value.to_string());
                }
            }
        }

        let mut result = HashMap::new();
        for provider in provider_names {
            let env_key = format!("{}_API_KEY", provider.to_uppercase());
            if let Some(value) = parsed.get(&env_key) {
                result.insert(provider.clone(), value.clone());
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn windows_drive_mounts_are_mnt_and_one_letter() {
        use std::path::Path;
        assert!(is_windows_drive_mount(Path::new(
            "/mnt/c/Users/u/secrets.env"
        )));
        assert!(is_windows_drive_mount(Path::new("/mnt/E/x")));
        assert!(!is_windows_drive_mount(Path::new(
            "/mnt/storage/secrets.env"
        )));
        assert!(!is_windows_drive_mount(Path::new(
            "/home/u/.config/coder/secrets.env"
        )));
    }

    #[cfg(unix)]
    fn set_owner_only(path: &std::path::Path) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(not(unix))]
    fn set_owner_only(_path: &std::path::Path) {}

    #[test]
    fn file_source_loads_only_requested_providers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        std::fs::write(&path, "ANTHROPIC_API_KEY=abc\nNIM_API_KEY=def\n").unwrap();
        set_owner_only(&path);
        let source = FileSource { path };
        let keys = source.load(&["anthropic".into()]).unwrap();
        assert_eq!(keys.get("anthropic"), Some(&"abc".to_string()));
        assert!(!keys.contains_key("nim"));
    }

    #[test]
    fn file_source_skips_blank_values() {
        // Regression: a line like `ANTHROPIC_API_KEY=` (never filled in) was still
        // inserted with an empty value, so callers checking `contains_key` (e.g.
        // `coder doctor`'s key-status report) wrongly reported the key as loaded.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        std::fs::write(&path, "ANTHROPIC_API_KEY=\nNIM_API_KEY=def\n").unwrap();
        set_owner_only(&path);
        let source = FileSource { path };
        let keys = source.load(&["anthropic".into(), "nim".into()]).unwrap();
        assert!(
            !keys.contains_key("anthropic"),
            "blank value should not count as loaded"
        );
        assert_eq!(keys.get("nim"), Some(&"def".to_string()));
    }

    #[test]
    fn file_source_strips_matching_surrounding_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        std::fs::write(&path, "ANTHROPIC_API_KEY=\"abc\"\nNIM_API_KEY='def'\n").unwrap();
        set_owner_only(&path);
        let source = FileSource { path };
        let keys = source.load(&["anthropic".into(), "nim".into()]).unwrap();
        assert_eq!(keys.get("anthropic"), Some(&"abc".to_string()));
        assert_eq!(keys.get("nim"), Some(&"def".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn file_source_rejects_loose_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        std::fs::write(&path, "ANTHROPIC_API_KEY=abc\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let source = FileSource { path };
        let err = source.load(&["anthropic".into()]).unwrap_err().to_string();
        assert!(
            err.contains("600"),
            "error should state the required chmod: {err}"
        );
    }
}

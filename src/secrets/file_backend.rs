use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

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

/// Reads provider API keys from a plaintext `KEY=value` file (e.g. `ANTHROPIC_API_KEY=abc`).
/// Refuses to read the file at all unless its permissions are exactly owner read/write (0600).
pub struct FileSource {
    pub path: PathBuf,
}

impl SecretsSource for FileSource {
    fn load(&self, provider_names: &[String]) -> anyhow::Result<HashMap<String, String>> {
        // Check permissions on the open fd, not the path, to avoid a swap/chmod race.
        let mut file = File::open(&self.path)?;
        #[cfg(unix)]
        {
            let mode = file.metadata()?.permissions().mode();
            if mode & 0o077 != 0 {
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
                parsed.insert(key.trim().to_string(), value.to_string());
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

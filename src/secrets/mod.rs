mod file_backend;

pub use file_backend::FileSource;

use std::collections::HashMap;

/// A source of provider API keys, keyed by provider name.
pub trait SecretsSource {
    /// # Errors
    ///
    /// Returns an error if the keys can't be loaded.
    fn load(&self, provider_names: &[String]) -> anyhow::Result<HashMap<String, String>>;
}

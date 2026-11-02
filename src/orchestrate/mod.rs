pub mod guard;
pub mod podman;
pub mod project_root;
pub mod select;

/// Sorted, comma-joined names, for error/listing messages naming the configured
/// providers/tools/etc. (e.g. "unknown provider `x`; configured providers: a, b, c").
#[must_use]
pub fn sorted_joined<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let mut names: Vec<&str> = names.map(String::as_str).collect();
    names.sort_unstable();
    names.join(", ")
}

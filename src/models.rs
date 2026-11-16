use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use coder::config::providers::{ProviderConfig, WireFormat};
use coder::orchestrate::sorted_joined;
use coder::proxy::translator::upstream_url;

/// Model ids that are almost never a general-purpose chat model, matched against a
/// provider's `/v1/models` id when that provider gives us no capability metadata to
/// filter on directly (e.g. NIM). Best-effort name heuristic, not a guarantee.
const NON_CHAT_NAME_HINTS: &[&str] = &[
    "embed",
    "rerank",
    "guard",
    "safety",
    "topic-control",
    "contentsafety",
    "retriever",
    "diffusiongemma",
    "deplot",
    "fuyu",
    "reward",
    "parse",
    "translate",
    "detector",
    "nvclip",
    "calibration",
    "kosmos",
];

fn is_likely_chat_model(id: &str) -> bool {
    let lower = id.to_lowercase();
    !NON_CHAT_NAME_HINTS.iter().any(|hint| lower.contains(hint))
}

#[derive(Deserialize)]
struct GenericModelList {
    data: Vec<GenericModel>,
}

#[derive(Deserialize)]
struct GenericModel {
    id: String,
}

#[derive(Deserialize)]
struct OpenRouterModelList {
    data: Vec<OpenRouterModel>,
}

#[derive(Deserialize)]
struct OpenRouterModel {
    id: String,
    architecture: Option<OpenRouterArchitecture>,
    pricing: Option<OpenRouterPricing>,
}

#[derive(Deserialize)]
struct OpenRouterArchitecture {
    output_modalities: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct OpenRouterPricing {
    prompt: Option<String>,
}

/// Fetch `{upstream}/models` using the provider's own configured auth header (not
/// always `Authorization: Bearer`; Anthropic, for example, needs `x-api-key` with no
/// prefix) and return the raw JSON body.
/// `upstream` as reachable from the host: `host.containers.internal` (and Docker's
/// `host.docker.internal`) only resolve inside containers, and both mean this host.
fn host_side_url(upstream: &str) -> String {
    match reqwest::Url::parse(upstream) {
        Ok(mut url)
            if matches!(
                url.host_str(),
                Some("host.containers.internal" | "host.docker.internal")
            ) =>
        {
            let _ = url.set_host(Some("localhost"));
            url.to_string()
        }
        _ => upstream.to_string(),
    }
}

/// Fetch `provider`'s model list, building the URL the way the proxy does. Without
/// `api_key` no auth header is sent, as the proxy does for keyless providers like Ollama.
pub fn fetch_models_json(provider: &ProviderConfig, api_key: Option<&str>) -> Result<Value> {
    let url = upstream_url(&host_side_url(&provider.upstream), "/v1/models");
    let mut request = reqwest::blocking::Client::new().get(&url);
    if provider.wire_format == WireFormat::AnthropicMessages {
        request = request.header("anthropic-version", "2023-06-01");
    }
    if let Some(api_key) = api_key {
        let value = if provider.auth_header_prefix.is_empty() {
            api_key.to_string()
        } else {
            format!("{} {api_key}", provider.auth_header_prefix)
        };
        request = request.header(&provider.auth_header_name, value);
    }
    let resp = request
        .send()
        .with_context(|| format!("fetching model list from {url}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .with_context(|| format!("reading model list response from {url}"))?;
    if !status.is_success() {
        // Check status before attempting to parse: an error response is often plain
        // text or HTML (e.g. from a gateway in front of the real API), and trying to
        // JSON-parse that first would surface a confusing parse error instead of the
        // actual HTTP failure (e.g. an invalid key).
        let hint = if api_key.is_none() && matches!(status.as_u16(), 401 | 403) {
            " (no key is loaded for this provider; add it to your secrets file or pass --secrets-file)"
        } else {
            ""
        };
        anyhow::bail!("{url} returned {status}{hint}: {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("parsing model list response from {url}"))
}

/// Filter a provider's raw `/v1/models` JSON down to likely chat-capable model ids,
/// using provider-specific metadata when available (`OpenRouter`) or a name heuristic
/// otherwise (NIM and anything else with no capability field).
pub fn filter_chat_models(provider_name: &str, raw: &Value) -> Result<Vec<String>> {
    if provider_name == "openrouter" {
        let parsed: OpenRouterModelList =
            serde_json::from_value(raw.clone()).context("parsing OpenRouter model list")?;
        let mut models: Vec<(String, bool)> = parsed
            .data
            .into_iter()
            .filter(|m| {
                m.architecture
                    .as_ref()
                    .and_then(|a| a.output_modalities.as_ref())
                    .is_some_and(|mods| mods.iter().any(|m| m == "text"))
            })
            .map(|m| {
                let is_free = m
                    .pricing
                    .as_ref()
                    .and_then(|p| p.prompt.as_ref())
                    .is_some_and(|p| p.parse::<f64>().unwrap_or(f64::MAX) == 0.0);
                (m.id, is_free)
            })
            .collect();
        // Free models first, otherwise preserve the catalog's own ordering.
        models.sort_by_key(|(_, is_free)| !*is_free);
        Ok(models.into_iter().map(|(id, _)| id).collect())
    } else {
        let parsed: GenericModelList =
            serde_json::from_value(raw.clone()).context("parsing model list")?;
        Ok(parsed
            .data
            .into_iter()
            .map(|m| m.id)
            .filter(|id| is_likely_chat_model(id))
            .collect())
    }
}

/// `~/.config/coder/models/<provider>.json`: a shared, flat JSON array of model ids,
/// generated by `coder models refresh` and consumed by both `coder models list` (for
/// tools like aider/claude that take `--model` directly) and the opencode.jsonc model
/// list regenerated from it.
pub fn shared_models_path(config_dir: &Path, provider: &str) -> PathBuf {
    config_dir.join("models").join(format!("{provider}.json"))
}

pub fn write_shared_list(path: &Path, ids: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(ids)?;
    fs::write(path, json).with_context(|| format!("writing {}", path.display()))
}

pub fn read_shared_list(path: &Path) -> Result<Vec<String>> {
    let text = fs::read_to_string(path).with_context(|| {
        format!(
            "reading {} (run `coder models refresh` first)",
            path.display()
        )
    })?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Read every `<config_dir>/models/*.json` shared list and return a `provider -> ids` map,
/// for regenerating opencode.jsonc's merged model list across all refreshed providers.
pub fn read_all_shared_lists(config_dir: &Path) -> Result<HashMap<String, Vec<String>>> {
    let dir = config_dir.join("models");
    let mut out = HashMap::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let provider = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        out.insert(provider, read_shared_list(&path)?);
    }
    Ok(out)
}

fn humanize(id: &str) -> String {
    id.rsplit('/').next().unwrap_or(id).replace(['-', '_'], " ")
}

/// Fully regenerate `opencode.jsonc`'s `provider.coder.models` map from every
/// provider's shared list under `<config_dir>/models/`. This OVERWRITES the file:
/// any hand-tuned per-model `options`/`variants` you'd added are not preserved across
/// a refresh; re-apply them after if you want them back.
pub fn regenerate_opencode_config(opencode_jsonc_path: &Path, config_dir: &Path) -> Result<()> {
    let all = read_all_shared_lists(config_dir)?;

    // An id can appear in more than one provider's catalog; label it with every
    // provider it came from so the picker shows which upstream(s) will accept it.
    // your session is locked to one provider at `coder run --provider`, so a model
    // label naming a DIFFERENT provider than your session will fail if you pick it.
    let mut providers_by_id: HashMap<String, Vec<String>> = HashMap::new();
    for (provider, ids) in &all {
        for id in ids {
            providers_by_id
                .entry(id.clone())
                .or_default()
                .push(provider.clone());
        }
    }
    let mut ids: Vec<&String> = providers_by_id.keys().collect();
    ids.sort();

    // keep `coder/default` as the default: the proxy maps it to the provider's default_model
    let mut models_json = serde_json::Map::new();
    models_json.insert(
        "default".to_string(),
        serde_json::json!({ "name": "coder session model (the provider's default_model)" }),
    );
    models_json.extend(ids.iter().map(|id| {
        let mut tags = providers_by_id[*id].clone();
        tags.sort();
        let label = format!("[{}] {}", tags.join(", "), humanize(id));
        ((*id).clone(), serde_json::json!({ "name": label }))
    }));

    let doc = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "model": "coder/default",
        "permission": {
            "bash": {
                "*": "ask",
                "git status*": "allow",
                "git diff*": "allow",
                "git log*": "allow",
                "ls*": "allow",
                "cat*": "allow",
                "rm -rf /*": "deny",
                "rm -rf .": "deny",
                "rm -rf ~*": "deny",
                "curl*|*sh": "deny"
            },
            "edit": "ask"
        },
        "provider": {
            "coder": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "coder proxy",
                "options": {
                    "baseURL": "{env:OPENAI_API_BASE}",
                    "apiKey": "{env:OPENAI_API_KEY}"
                },
                "models": models_json
            }
        }
    });

    let header = "// Regenerated by `coder models refresh`. This file is OVERWRITTEN on\n\
         // every refresh. Hand-edited per-model `options`/`variants` do not survive\n\
         // a refresh; re-apply them after if you want them back.\n\
         // Model ids aren't provider-aware: switching `coder run --provider` means\n\
         // picking a model valid for THAT provider. Run with no `--model` flag so the\n\
         // proxy doesn't override your /models choice (see main.rs's session_model).\n";
    let body = serde_json::to_string_pretty(&doc)?;
    fs::write(opencode_jsonc_path, format!("{header}{body}\n"))
        .with_context(|| format!("writing {}", opencode_jsonc_path.display()))
}

/// Fetches, filters, and writes one provider's model list. Does NOT regenerate
/// opencode.jsonc, so callers refreshing several providers can do that once at the end.
pub fn refresh_one_provider(
    provider_name: &str,
    providers: &HashMap<String, ProviderConfig>,
    real_keys: &HashMap<String, String>,
    config_dir: &Path,
) -> Result<()> {
    let provider = providers.get(provider_name).ok_or_else(|| {
        let list = sorted_joined(providers.keys());
        anyhow::anyhow!("unknown provider `{provider_name}`; configured providers: {list}")
    })?;
    let raw = fetch_models_json(provider, real_keys.get(provider_name).map(String::as_str))?;
    let filtered = filter_chat_models(provider_name, &raw)?;
    println!(
        "{} candidate chat models for `{provider_name}`",
        filtered.len()
    );

    let list_path = shared_models_path(config_dir, provider_name);
    write_shared_list(&list_path, &filtered)?;
    println!("wrote {}", list_path.display());
    Ok(())
}

/// Refreshes every configured provider, collecting (not aborting on) per-provider
/// failures, then regenerates opencode.jsonc once at the end regardless.
pub fn refresh_all_providers(
    providers: &HashMap<String, ProviderConfig>,
    real_keys: &HashMap<String, String>,
    config_dir: &Path,
    opencode_jsonc_path: &Path,
) -> Result<()> {
    let mut names: Vec<&String> = providers.keys().collect();
    names.sort();
    let mut failed = Vec::new();
    for name in names {
        if let Err(e) = refresh_one_provider(name, providers, real_keys, config_dir) {
            eprintln!("warning: skipping `{name}`: {e:#}");
            failed.push(name.clone());
        }
    }
    regenerate_opencode_and_report(config_dir, opencode_jsonc_path)?;
    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("failed to refresh: {}", failed.join(", "));
    }
}

pub fn regenerate_opencode_and_report(config_dir: &Path, opencode_jsonc_path: &Path) -> Result<()> {
    regenerate_opencode_config(opencode_jsonc_path, config_dir)?;
    println!(
        "regenerated {} (run `coder build` to apply it)",
        opencode_jsonc_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn generic_filter_excludes_non_chat_hints() {
        let raw = json!({
            "data": [
                {"id": "meta/muse-glimmer-30b"},
                {"id": "nvidia/llama-3.1-nemoguard-8b-content-safety"},
                {"id": "nvidia/nemotron-4-340b-reward"},
                {"id": "nvidia/riva-translate-4b-instruct"},
                {"id": "microsoft/kosmos-2"},
            ]
        });
        let kept = filter_chat_models("nim", &raw).unwrap();
        assert_eq!(kept, vec!["meta/muse-glimmer-30b"]);
    }

    #[test]
    fn openrouter_filter_keeps_only_text_output_and_sorts_free_first() {
        let raw = json!({
            "data": [
                {"id": "paid-text-model", "architecture": {"output_modalities": ["text"]}, "pricing": {"prompt": "0.000001"}},
                {"id": "image-model", "architecture": {"output_modalities": ["image"]}, "pricing": {"prompt": "0"}},
                {"id": "free-text-model", "architecture": {"output_modalities": ["text"]}, "pricing": {"prompt": "0"}},
            ]
        });
        let kept = filter_chat_models("openrouter", &raw).unwrap();
        assert_eq!(kept, vec!["free-text-model", "paid-text-model"]);
    }

    #[test]
    fn shared_list_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = shared_models_path(dir.path(), "nim");
        let ids = vec!["a/b".to_string(), "c/d".to_string()];
        write_shared_list(&path, &ids).unwrap();
        assert_eq!(read_shared_list(&path).unwrap(), ids);
    }

    #[test]
    fn regenerate_opencode_config_includes_every_provider_model() {
        let config_dir = tempfile::tempdir().unwrap();
        write_shared_list(
            &shared_models_path(config_dir.path(), "nim"),
            &["meta/a".to_string()],
        )
        .unwrap();
        write_shared_list(
            &shared_models_path(config_dir.path(), "openrouter"),
            &["openai/b".to_string()],
        )
        .unwrap();
        let out = config_dir.path().join("opencode.jsonc");
        regenerate_opencode_config(&out, config_dir.path()).unwrap();
        let written = fs::read_to_string(&out).unwrap();
        assert!(written.contains("meta/a"));
        assert!(written.contains("openai/b"));
        assert!(written.contains("{env:OPENAI_API_BASE}"));
        let doc: Value = serde_json::from_str(
            &written
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        assert_eq!(doc["model"], "coder/default");
        assert!(doc["provider"]["coder"]["models"]["default"].is_object());
    }

    #[test]
    fn read_all_shared_lists_merges_every_provider_file() {
        let dir = tempfile::tempdir().unwrap();
        write_shared_list(
            &shared_models_path(dir.path(), "nim"),
            &["nim/a".to_string()],
        )
        .unwrap();
        write_shared_list(
            &shared_models_path(dir.path(), "openrouter"),
            &["or/b".to_string()],
        )
        .unwrap();
        let all = read_all_shared_lists(dir.path()).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all["nim"], vec!["nim/a".to_string()]);
        assert_eq!(all["openrouter"], vec!["or/b".to_string()]);
    }

    fn openai(upstream: &str) -> ProviderConfig {
        ProviderConfig {
            upstream: upstream.to_string(),
            wire_format: WireFormat::OpenAiChatCompletions,
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer".to_string(),
            path_override: None,
            default_model: None,
        }
    }

    #[test]
    fn anthropic_format_upstreams_get_v1_models_and_a_version_header() {
        let (url, server) = one_shot_server("200 OK");
        let mut p = openai(url.trim_end_matches("/v1"));
        p.wire_format = WireFormat::AnthropicMessages;
        fetch_models_json(&p, None).unwrap();
        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /v1/models "), "{request}");
        assert!(
            request.contains("anthropic-version: 2023-06-01"),
            "{request}"
        );
    }

    /// Serve one HTTP request on a local port with `status`, returning the base URL and
    /// a handle that yields the raw request text.
    fn one_shot_server(status: &str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let status = status.to_string();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let body = r#"{"data":[{"id":"m1"}]}"#;
            write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (url, handle)
    }

    #[test]
    fn fetch_without_a_key_sends_no_auth_header() {
        let (url, server) = one_shot_server("200 OK");
        let raw = fetch_models_json(&openai(&url), None).unwrap();
        assert_eq!(raw["data"][0]["id"], "m1");
        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /v1/models "), "{request}");
        assert!(!request.contains("authorization:"), "{request}");
    }

    #[test]
    fn fetch_with_a_key_sends_it_with_the_provider_prefix() {
        let (url, server) = one_shot_server("200 OK");
        let mut p = openai(&url);
        p.auth_header_name = "x-api-key".into();
        p.auth_header_prefix = String::new();
        fetch_models_json(&p, Some("k1")).unwrap();
        assert!(server
            .join()
            .unwrap()
            .to_ascii_lowercase()
            .contains("x-api-key: k1"));
    }

    #[test]
    fn keyless_auth_failure_hints_that_a_key_is_needed() {
        let (url, server) = one_shot_server("401 Unauthorized");
        let err = fetch_models_json(&openai(&url), None).unwrap_err();
        server.join().unwrap();
        assert!(err.to_string().contains("no key is loaded"), "{err}");
    }

    #[test]
    fn container_only_host_names_become_localhost_on_the_host() {
        assert_eq!(
            host_side_url("http://host.containers.internal:11434/v1"),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            host_side_url("http://host.docker.internal:11434/v1"),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            host_side_url("https://openrouter.ai/api/v1"),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(host_side_url("not a url"), "not a url");
    }
}

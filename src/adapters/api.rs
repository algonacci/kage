//! API-backed adapters: one HTTP call instead of a spawned harness.
//!
//! This exists so a role can be filled by a model the user pays for by the token rather than by a
//! subscription CLI, and so the two can be mixed in one workflow — an API planner with a CLI
//! executor is a first-class setup, not a fallback.
//!
//! It is deliberately not an agent. A completions endpoint returns text; it cannot read a
//! repository, edit a file, or run a test. That is why only the planner and the reviewer can be
//! backed this way: Kage already assembles their entire context from artifacts, and their
//! deliverable is prose, so Kage writes the artifact from the response. Giving the executor the
//! same treatment would mean building a tool loop here, which is the one thing Kage exists not to
//! do — `Config::validate` rejects that configuration rather than half-supporting it.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::adapters::{AgentAdapter, AgentRequest, AgentResult, Role};
use crate::config::{Provider, RoleConfig};

/// Calls an OpenAI-compatible chat completions endpoint.
#[derive(Debug)]
pub struct ApiAdapter {
    role: Role,
    provider_name: String,
    base_url: String,
    model: String,
    /// Read once at construction so a missing key fails before a run starts rather than at the
    /// phase that needs it. Never logged, never included in an error.
    api_key: String,
    timeout: Duration,
}

impl ApiAdapter {
    pub fn new(
        role: Role,
        provider_name: &str,
        provider: &Provider,
        config: &RoleConfig,
    ) -> Result<Self> {
        let model = config
            .model
            .clone()
            .context("an API-backed role must name a model")?;

        let api_key = std::env::var(&provider.api_key_env).map_err(|_| {
            anyhow::anyhow!(
                "environment variable `{}` is not set — provider `{provider_name}` needs it for \
                 the {role} role",
                provider.api_key_env
            )
        })?;

        if api_key.trim().is_empty() {
            bail!(
                "environment variable `{}` is set but empty",
                provider.api_key_env
            );
        }

        Ok(Self {
            role,
            provider_name: provider_name.to_string(),
            base_url: provider.base_url.trim_end_matches('/').to_string(),
            model,
            api_key,
            timeout: Duration::from_secs(config.timeout_secs),
        })
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

/// The request body for a completion.
///
/// Split out from the call so the wire shape can be tested without a network or a key.
fn request_body(model: &str, prompt: &str) -> Value {
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        // No temperature or max_tokens: the provider's own defaults are likelier to suit a model
        // than a number guessed here, and a cap set too low would truncate a plan mid-sentence.
        "stream": false,
    })
}

/// Pull the assistant's text out of a completions response.
///
/// Providers vary in what they add around it but agree on this path. A response that does not
/// match is reported with its shape rather than silently becoming an empty artifact — an empty
/// `PLAN.md` fails three phases later with no clue why.
fn extract_content(body: &Value) -> Result<String> {
    if let Some(message) = body["error"]["message"].as_str() {
        bail!("the provider returned an error: {message}");
    }

    let choice = body["choices"]
        .get(0)
        .context("the response contained no choices")?;

    let content = choice["message"]["content"]
        .as_str()
        .context("the response had no message content")?;

    if content.trim().is_empty() {
        bail!("the model returned an empty response");
    }

    Ok(content.to_string())
}

/// Strip anything key-shaped from text that is about to be shown to a user.
///
/// A transport error can quote the request it failed on, headers included. This runs on every
/// error message that leaves the adapter, because a key printed to a terminal is a key in a
/// screenshot, a scrollback buffer, and a pasted issue.
fn redact(text: &str, api_key: &str) -> String {
    if api_key.is_empty() {
        return text.to_string();
    }
    text.replace(api_key, "<redacted>")
}

#[async_trait]
impl AgentAdapter for ApiAdapter {
    async fn run(&self, request: AgentRequest) -> Result<AgentResult> {
        // Written for the same reason the CLI adapter writes it: when a run goes wrong, the first
        // question is what the model was actually told.
        if let Some(parent) = request.prompt_file.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        std::fs::write(&request.prompt_file, &request.prompt)
            .with_context(|| format!("cannot write {}", request.prompt_file.display()))?;

        println!(
            "  [{}] calling {} ({})",
            request.label, self.provider_name, self.model
        );

        let started = Instant::now();
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .context("cannot build an HTTP client")?;

        let response = client
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            .json(&request_body(&self.model, &request.prompt))
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("{}", redact(&error.to_string(), &self.api_key)))
            .with_context(|| format!("cannot reach provider `{}`", self.provider_name))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| anyhow::anyhow!("{}", redact(&error.to_string(), &self.api_key)))
            .context("cannot read the provider's response")?;

        let duration_secs = started.elapsed().as_secs();

        if !status.is_success() {
            // Reported as a failed result rather than an error, so the engine treats it exactly
            // like a harness that exited non-zero and the run ends with a phase summary.
            let detail = redact(text.trim(), &self.api_key);
            let detail: String = detail.chars().take(600).collect();
            return Ok(AgentResult {
                code: Some(1),
                stdout: String::new(),
                stderr: format!("provider returned HTTP {status}: {detail}"),
                timed_out: false,
                stalled: false,
                duration_secs,
            });
        }

        let body: Value = serde_json::from_str(&text)
            .context("the provider's response was not JSON")
            .map_err(|error| anyhow::anyhow!("{}", redact(&error.to_string(), &self.api_key)))?;

        match extract_content(&body) {
            Ok(content) => {
                let log = format!(
                    "$ POST {} ({})\n\n--- response ---\n{content}\n",
                    self.endpoint(),
                    self.model
                );
                if let Some(parent) = request.log_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&request.log_path, log);

                println!(
                    "  [{}] {} chars in {duration_secs}s",
                    request.label,
                    content.len()
                );

                Ok(AgentResult {
                    code: Some(0),
                    stdout: content,
                    stderr: String::new(),
                    timed_out: false,
                    stalled: false,
                    duration_secs,
                })
            }
            Err(error) => Ok(AgentResult {
                code: Some(1),
                stdout: String::new(),
                stderr: redact(&format!("{error:#}"), &self.api_key),
                timed_out: false,
                stalled: false,
                duration_secs,
            }),
        }
    }

    fn describe(&self) -> String {
        format!(
            "{} via api ({} · {})",
            self.role, self.provider_name, self.model
        )
    }

    /// Kage writes this role's artifact from the response: an endpoint cannot touch the filesystem.
    fn writes_own_artifacts(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_names_the_model_and_carries_the_prompt_as_one_user_message() {
        let body = request_body("anthropic/claude-opus-5", "plan this");

        assert_eq!(body["model"], "anthropic/claude-opus-5");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "plan this");
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn content_is_taken_from_the_first_choice() {
        let body = json!({
            "choices": [{ "message": { "role": "assistant", "content": "# Objective\nship it" } }]
        });

        assert_eq!(extract_content(&body).unwrap(), "# Objective\nship it");
    }

    #[test]
    fn a_provider_error_is_reported_with_its_own_message() {
        let body = json!({ "error": { "message": "model not found", "type": "invalid_request" } });

        let error = extract_content(&body).unwrap_err().to_string();

        assert!(error.contains("model not found"), "{error}");
    }

    #[test]
    fn an_empty_completion_is_an_error_rather_than_an_empty_artifact() {
        // Silently writing an empty PLAN.md fails three phases later with no clue why.
        let body = json!({ "choices": [{ "message": { "content": "   " } }] });

        assert!(extract_content(&body).is_err());
    }

    #[test]
    fn an_unrecognised_shape_says_so_instead_of_returning_nothing() {
        assert!(extract_content(&json!({ "choices": [] })).is_err());
        assert!(extract_content(&json!({ "unexpected": true })).is_err());
    }

    #[test]
    fn the_key_never_survives_into_a_message() {
        let key = "sk-live-supersecret";
        let leaked = format!("error sending request with header authorization: Bearer {key}");

        let safe = redact(&leaked, key);

        assert!(!safe.contains(key), "{safe}");
        assert!(safe.contains("<redacted>"));
    }

    #[test]
    fn redaction_is_a_no_op_without_a_key() {
        assert_eq!(redact("plain message", ""), "plain message");
    }

    #[test]
    fn a_missing_environment_variable_fails_at_construction() {
        let provider = Provider {
            base_url: "https://example.invalid/v1".to_string(),
            api_key_env: "KAGE_TEST_KEY_THAT_IS_NOT_SET".to_string(),
            kind: crate::config::schema::ProviderKind::OpenaiCompatible,
        };
        let mut role = RoleConfig::preset(crate::config::AdapterKind::Api);
        role.model = Some("some-model".to_string());

        let error = ApiAdapter::new(Role::Planner, "test", &provider, &role)
            .unwrap_err()
            .to_string();

        assert!(error.contains("KAGE_TEST_KEY_THAT_IS_NOT_SET"), "{error}");
        assert!(error.contains("planner"), "{error}");
    }

    #[test]
    fn the_endpoint_is_built_without_a_doubled_slash() {
        // A base_url pasted with its trailing slash must not produce `/v1//chat/completions`.
        let provider = Provider {
            base_url: "https://api.example.com/v1/".to_string(),
            api_key_env: "KAGE_TEST_KEY_PRESENT".to_string(),
            kind: crate::config::schema::ProviderKind::OpenaiCompatible,
        };
        let mut role = RoleConfig::preset(crate::config::AdapterKind::Api);
        role.model = Some("m".to_string());

        // SAFETY: single-threaded test scope, and the variable is unique to this test.
        unsafe { std::env::set_var("KAGE_TEST_KEY_PRESENT", "k") };
        let adapter = ApiAdapter::new(Role::Reviewer, "example", &provider, &role).unwrap();
        unsafe { std::env::remove_var("KAGE_TEST_KEY_PRESENT") };

        assert_eq!(
            adapter.endpoint(),
            "https://api.example.com/v1/chat/completions"
        );
        assert!(!adapter.writes_own_artifacts());
        assert!(adapter.describe().contains("example"));
    }
}

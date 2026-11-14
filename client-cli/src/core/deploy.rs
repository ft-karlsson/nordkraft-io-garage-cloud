//! Deploy and upgrade. The input is fully resolved before it gets here: no
//! `--from`, no env file, no prompts — those belong to the presentation.

use std::collections::HashMap;
use std::fmt;

use super::aliases::resolve_alias;
use super::api::*;
use super::registry::resolve_registry_image;
use super::spec::*;
use super::CoreResult;

/// Everything one deploy needs, already resolved.
#[derive(Debug, Clone)]
pub(crate) struct DeployInput {
    pub(crate) image: String,
    pub(crate) ports: Vec<u16>,
    pub(crate) env: HashMap<String, String>,
    pub(crate) cpu: f32,
    pub(crate) memory: String,
    pub(crate) persistence: bool,
    pub(crate) volume_path: Option<String>,
    pub(crate) volume_size: String,
    pub(crate) ipv6: bool,
    pub(crate) garage: Option<String>,
    pub(crate) hardware: Option<String>,
    /// Shell-style command line; tokenised when the request is built.
    pub(crate) command: Option<String>,
    pub(crate) lifecycle: LifecycleSpec,
    pub(crate) labels: HashMap<String, String>,
    /// Answer only once the node reports the container running (or failed).
    /// Needed before anything that acts on the container right after.
    pub(crate) wait: bool,
}

impl Default for DeployInput {
    fn default() -> Self {
        DeployInput {
            image: String::new(),
            ports: Vec::new(),
            env: HashMap::new(),
            cpu: 0.5,
            memory: "512m".to_string(),
            persistence: false,
            volume_path: None,
            volume_size: default_volume_size(),
            ipv6: false,
            garage: None,
            hardware: None,
            command: None,
            lifecycle: LifecycleSpec::default(),
            labels: HashMap::new(),
            wait: false,
        }
    }
}

/// Read `KEY=VALUE` lines from an env file. Blank lines and `#` comments are
/// skipped; keys and values are trimmed.
pub(crate) fn parse_env_file(path: &str) -> CoreResult<HashMap<String, String>> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot read env file '{}': {}", path, e))?;
    let mut env = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            env.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    Ok(env)
}

/// Add `KEY=VALUE` pairs (as given on the command line) to `env`.
pub(crate) fn parse_env_pairs(
    pairs: &[String],
    env: &mut HashMap<String, String>,
) -> CoreResult<()> {
    for pair in pairs {
        if let Some((key, value)) = pair.split_once('=') {
            env.insert(key.to_string(), value.to_string());
        } else {
            return Err(format!("Invalid env format '{}'. Use KEY=VALUE", pair).into());
        }
    }
    Ok(())
}

/// Shell-style tokenising that respects quoted strings. `split_whitespace()`
/// breaks on spaces inside quotes, mangling e.g.
/// `node -e "require('http').createServer(...).listen(3000)"`.
pub(crate) fn tokenize_command(c: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = ' ';
    let mut chars = c.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' | '\'' if !in_quotes => {
                in_quotes = true;
                quote_char = ch;
            }
            ch if in_quotes && ch == quote_char => {
                in_quotes = false;
            }
            '\\' if in_quotes => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ' ' | '\t' if !in_quotes => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// A sensible default port from the image name: web servers 80, Node and
/// Rails 3000, Python frameworks 8000, Java and Go 8080. Databases expose no
/// HTTP port, so None. Anything unknown falls back to 80.
pub(crate) fn suggest_port(image: &str) -> Option<u16> {
    let img = image.to_lowercase();
    if img.contains("nginx")
        || img.contains("apache")
        || img.contains("caddy")
        || img.contains("traefik")
    {
        Some(80)
    } else if img.contains("node")
        || img.contains("express")
        || img.contains("next")
        || img.contains("rails")
        || img.contains("campfire")
        || img.contains("ruby")
    {
        Some(3000)
    } else if img.contains("django")
        || img.contains("flask")
        || img.contains("fastapi")
        || img.contains("python")
    {
        Some(8000)
    } else if img.contains("spring")
        || img.contains("java")
        || img.contains("tomcat")
        || img.contains("go")
        || img.contains("golang")
    {
        Some(8080)
    } else if img.contains("postgres")
        || img.contains("mysql")
        || img.contains("mongo")
        || img.contains("redis")
    {
        None // DB images — no HTTP port
    } else {
        Some(80) // generic fallback
    }
}

/// The one rule the controller does not check for us.
pub(crate) fn validate(input: &DeployInput) -> CoreResult<()> {
    if input.persistence && input.volume_path.is_none() {
        return Err("--persistence requires --volume-path (e.g., --volume-path /data or --volume-path /rails/storage)".into());
    }
    Ok(())
}

/// Turn the input into the controller's request. Resolves `registry://`
/// images and tokenises the command.
pub(crate) fn build_request(input: &DeployInput) -> CoreResult<DeployRequest> {
    Ok(DeployRequest {
        image: resolve_registry_image(&input.image)?,
        ports: input
            .ports
            .iter()
            .map(|p| PortSpec {
                port: *p,
                protocol: "tcp".to_string(),
            })
            .collect(),
        command: input.command.as_deref().map(tokenize_command),
        env_vars: input.env.clone(),
        cpu_limit: input.cpu,
        memory_limit: input.memory.clone(),
        enable_persistence: input.persistence,
        volume_path: input.volume_path.clone(),
        volume_size: if input.persistence {
            Some(input.volume_size.clone())
        } else {
            None
        },
        enable_ipv6: input.ipv6,
        target_garage: input.garage.clone(),
        hardware_preference: input.hardware.clone(),
        lifecycle: input.lifecycle.clone(),
        labels: input.labels.clone(),
        wait: input.wait,
        wait_timeout_s: if input.wait {
            Some(DEPLOY_WAIT_SECS)
        } else {
            None
        },
    })
}

/// Why a deploy did not happen.
#[derive(Debug)]
pub(crate) enum DeployError {
    /// The plan is full; `ApiError` carries the plan name and usage.
    Quota(ApiError),
    /// The controller refused with this message.
    Failed(String),
    /// Transport or decoding.
    Other(Box<dyn std::error::Error>),
}

impl fmt::Display for DeployError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeployError::Quota(e) => write!(f, "Quota exceeded: {}", e.error),
            DeployError::Failed(msg) => write!(f, "Deploy failed: {}", msg),
            DeployError::Other(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for DeployError {}

impl From<reqwest::Error> for DeployError {
    fn from(e: reqwest::Error) -> Self {
        DeployError::Other(Box::new(e))
    }
}

impl From<serde_json::Error> for DeployError {
    fn from(e: serde_json::Error) -> Self {
        DeployError::Other(Box::new(e))
    }
}

impl From<Box<dyn std::error::Error>> for DeployError {
    fn from(e: Box<dyn std::error::Error>) -> Self {
        DeployError::Other(e)
    }
}

/// `POST /containers/deploy`. The controller answers 200 for quota errors
/// too, so both the status code and the body's `error` field are checked.
pub(crate) async fn deploy(request: &DeployRequest) -> Result<DeployResponse, DeployError> {
    let client = if request.wait {
        crate::volume::transfer_client()?
    } else {
        create_client()?
    };
    let url = format!("{}/containers/deploy", *API_BASE_URL);
    let response = client.post(&url).json(request).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        if error.plan.is_some() || error.usage.is_some() {
            return Err(DeployError::Quota(error));
        }
        return Err(DeployError::Failed(error.error));
    }

    let body: serde_json::Value = response.json().await?;
    if let Some(err_msg) = body.get("error").and_then(|v| v.as_str()) {
        if body.get("plan").is_some() || body.get("usage").is_some() {
            return Err(DeployError::Quota(ApiError {
                error: err_msg.to_string(),
                plan: body
                    .get("plan")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                usage: body.get("usage").cloned(),
            }));
        }
        return Err(DeployError::Failed(err_msg.to_string()));
    }

    Ok(serde_json::from_value(body)?)
}

/// The `.nk` spec that records a deploy, revision 1.
pub(crate) fn spec_for(
    input: &DeployInput,
    container_name: &str,
    garage: Option<&str>,
) -> DeploymentSpec {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    DeploymentSpec {
        deployment: DeploymentMeta {
            name: container_name.to_string(),
            image: input.image.clone(),
            revision: 1,
            command: input.command.clone(),
            created: now.clone(),
            updated: now,
        },
        resources: ResourceSpec {
            cpu: input.cpu,
            memory: input.memory.clone(),
        },
        network: NetworkSpec {
            ports: input.ports.clone(),
            ipv6: input.ipv6,
        },
        storage: StorageSpec {
            persistence: input.persistence,
            volume_path: input.volume_path.clone(),
            volume_size: input.volume_size.clone(),
        },
        env: input.env.clone(),
        placement: PlacementSpec {
            garage: garage
                .map(|s| s.to_string())
                .or_else(|| input.garage.clone()),
            hardware: input.hardware.clone(),
        },
        lifecycle: input.lifecycle.clone(),
        labels: input.labels.clone(),
    }
}

// ============= UPGRADE =============

/// The full desired state from a `.nk` spec, as `PUT /containers/{name}/upgrade`
/// wants it. Resolves `registry://`.
pub(crate) fn upgrade_request(spec: &DeploymentSpec) -> CoreResult<CliUpgradeRequest> {
    Ok(CliUpgradeRequest {
        image: Some(resolve_registry_image(&spec.deployment.image)?),
        ports: if spec.network.ports.is_empty() {
            None
        } else {
            Some(
                spec.network
                    .ports
                    .iter()
                    .map(|p| PortSpec {
                        port: *p,
                        protocol: "tcp".to_string(),
                    })
                    .collect(),
            )
        },
        command: spec
            .deployment
            .command
            .as_ref()
            .map(|c| c.split_whitespace().map(|s| s.to_string()).collect()),
        env_vars: if spec.env.is_empty() {
            None
        } else {
            Some(spec.env.clone())
        },
        cpu_limit: Some(spec.resources.cpu),
        memory_limit: Some(spec.resources.memory.clone()),
        volume_path: spec.storage.volume_path.clone(),
        volume_size: Some(spec.storage.volume_size.clone()),
        lifecycle: Some(spec.lifecycle.clone()),
        labels: Some(spec.labels.clone()),
    })
}

/// Apply `spec` to the container. The controller reports API-level failures
/// as 200 with an `error` field; those come back as `Err` here.
pub(crate) async fn upgrade(name: &str, spec: &DeploymentSpec) -> CoreResult<UpgradeResponse> {
    let name = resolve_alias(name);
    let upgrade = upgrade_request(spec)?;
    let client = create_client()?;
    let url = format!("{}/containers/{}/upgrade", *API_BASE_URL, name);
    let response = client.put(&url).json(&upgrade).send().await?;

    let result: UpgradeResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse upgrade response: {}", e))?;

    if let Some(err) = &result.error {
        return Err(format!("Upgrade failed: {}", err).into());
    }
    Ok(result)
}

/// Record a successful upgrade in the local spec: the controller's revision
/// (or the next one) and a fresh `updated` stamp.
pub(crate) fn record_upgrade(spec: &DeploymentSpec, result: &UpgradeResponse) -> CoreResult<()> {
    let mut updated = spec.clone();
    updated.deployment.revision = result.revision.unwrap_or(updated.deployment.revision + 1);
    updated.deployment.updated =
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    save_deployment_spec(&updated)
}

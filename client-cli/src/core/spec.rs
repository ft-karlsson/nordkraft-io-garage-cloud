//! `.nk` deployment specs: the TOML schema, the files under
//! `~/.nordkraft/deployments/`, rebuilding a spec from a live or stored
//! container, and the spec ↔ live diff.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use toml_edit::{value, Array, DocumentMut, Item, Table, Value};

use super::api::*;
use super::config::*;
use super::CoreResult;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct DeploymentSpec {
    pub(crate) deployment: DeploymentMeta,
    pub(crate) resources: ResourceSpec,
    pub(crate) network: NetworkSpec,
    #[serde(default, skip_serializing_if = "StorageSpec::is_empty")]
    pub(crate) storage: StorageSpec,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) env: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "PlacementSpec::is_empty")]
    pub(crate) placement: PlacementSpec,
    /// Sandbox lifecycle: restart policy and the auto_stop / auto_delete /
    /// max_lifetime timers ("15m", "2h", "0" = never).
    #[serde(default, skip_serializing_if = "LifecycleSpec::is_empty")]
    pub(crate) lifecycle: LifecycleSpec,
    /// Free-form labels, filterable with `nordkraft list --label k=v`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) labels: HashMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
pub(crate) struct LifecycleSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) restart: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auto_stop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auto_delete: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_lifetime: Option<String>,
}

impl LifecycleSpec {
    pub(crate) fn is_empty(&self) -> bool {
        *self == LifecycleSpec::default()
    }

    pub(crate) fn from_flags(
        restart: &Option<String>,
        auto_stop: &Option<String>,
        auto_delete: &Option<String>,
        max_lifetime: &Option<String>,
    ) -> Self {
        LifecycleSpec {
            restart: restart.clone(),
            auto_stop: auto_stop.clone(),
            auto_delete: auto_delete.clone(),
            max_lifetime: max_lifetime.clone(),
        }
    }
}

/// "k=v" flags → map; a bare key gets an empty value.
pub(crate) fn labels_from_flags(flags: &[String]) -> HashMap<String, String> {
    flags
        .iter()
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (kv.clone(), String::new()),
        })
        .collect()
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct DeploymentMeta {
    pub(crate) name: String,
    pub(crate) image: String,
    #[serde(default)]
    pub(crate) revision: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) command: Option<String>,
    pub(crate) created: String,
    pub(crate) updated: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct ResourceSpec {
    pub(crate) cpu: f32,
    pub(crate) memory: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct NetworkSpec {
    #[serde(default)]
    pub(crate) ports: Vec<u16>,
    #[serde(default)]
    pub(crate) ipv6: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub(crate) struct StorageSpec {
    #[serde(default)]
    pub(crate) persistence: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) volume_path: Option<String>,
    #[serde(default = "default_volume_size")]
    pub(crate) volume_size: String,
}

pub(crate) fn default_volume_size() -> String {
    "1g".to_string()
}

impl StorageSpec {
    pub(crate) fn is_empty(&self) -> bool {
        !self.persistence && self.volume_path.is_none()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub(crate) struct PlacementSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) garage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) hardware: Option<String>,
}

impl PlacementSpec {
    pub(crate) fn is_empty(&self) -> bool {
        self.garage.is_none() && self.hardware.is_none()
    }
}

#[derive(Debug)]
pub(crate) struct SpecDiff {
    pub(crate) field: String,
    pub(crate) spec_value: String,
    pub(crate) live_value: String,
    pub(crate) kind: DiffKind,
}

#[derive(Debug, PartialEq)]
pub(crate) enum DiffKind {
    Changed,
    Added,   // in spec but not live
    Removed, // in live but not spec
    Same,
}

pub(crate) fn get_deployments_dir() -> PathBuf {
    get_config_dir().join(DEPLOYMENTS_DIR)
}

pub(crate) fn nk_path(name: &str) -> PathBuf {
    get_deployments_dir().join(format!("{}.nk", name))
}

pub(crate) fn save_deployment_spec(
    spec: &DeploymentSpec,
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = get_deployments_dir();
    std::fs::create_dir_all(&dir)?;
    let path = nk_path(&spec.deployment.name);
    let toml_str = toml::to_string_pretty(spec)?;
    let header = format!(
        "# NordKraft deployment spec — auto-generated\n# Edit and run: nordkraft upgrade {}\n\n",
        spec.deployment.name
    );
    std::fs::write(&path, format!("{}{}", header, toml_str))?;
    Ok(())
}

pub(crate) fn load_deployment_spec(name: &str) -> Option<DeploymentSpec> {
    let path = nk_path(name);
    if !path.exists() {
        return None;
    }
    let contents = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&contents).ok()
}

pub(crate) fn list_deployment_specs() -> Vec<String> {
    let dir = get_deployments_dir();
    if !dir.exists() {
        return vec![];
    }
    std::fs::read_dir(&dir)
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.strip_suffix(".nk").map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve a --from argument to a DeploymentSpec.
/// Accepts:
///   - Bare name: "my-campfire" → ~/.nordkraft/deployments/my-campfire.nk
///   - Path with extension: "./my-campfire.nk" or "/abs/path/spec.nk"
///   - Name with extension: "my-campfire.nk" → tries as path first, then deployments dir
pub(crate) fn resolve_and_load_spec(
    from: &str,
) -> Result<DeploymentSpec, Box<dyn std::error::Error>> {
    let path = PathBuf::from(from);

    // 1. If it's an existing file path, use it directly
    if path.exists() {
        let contents = std::fs::read_to_string(&path)?;
        let spec: DeploymentSpec =
            toml::from_str(&contents).map_err(|e| format!("Invalid .nk spec '{}': {}", from, e))?;
        return Ok(spec);
    }

    // 2. Try as a name in the deployments dir
    let bare_name = from.strip_suffix(".nk").unwrap_or(from);
    if let Some(spec) = load_deployment_spec(bare_name) {
        return Ok(spec);
    }

    // 3. Nothing found
    Err(format!(
        "Spec '{}' not found. Tried:\n  • {}\n  • {}\n\nAvailable specs: {}",
        from,
        path.display(),
        nk_path(bare_name).display(),
        {
            let specs = list_deployment_specs();
            if specs.is_empty() {
                "(none)".to_string()
            } else {
                specs.join(", ")
            }
        }
    )
    .into())
}

/// The limits the API could not read from the runtime (`cpu`, `memory`).
/// `spec_from_inspect` writes defaults for these — 0.5 cpu / 512m — which
/// keeps the spec valid, but a silent default is how a spec ends up capping
/// a container below what it actually runs with, so callers should say so.
pub(crate) fn unread_limits(c: &ContainerInspectResponse) -> Vec<&'static str> {
    let mut unread = Vec::new();
    if c.cpu_limit.is_none() {
        unread.push("cpu");
    }
    if c.memory_limit.is_none() {
        unread.push("memory");
    }
    unread
}

pub(crate) fn spec_from_inspect(c: &ContainerInspectResponse) -> DeploymentSpec {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Parse ports from inspect JSON
    let ports: Vec<u16> = c
        .ports
        .iter()
        .filter_map(|p| p["port"].as_u64().map(|v| v as u16))
        .collect();

    // Parse env vars (skip internal NK_ vars, HOME, PATH)
    let env: HashMap<String, String> = c
        .env_vars
        .iter()
        .filter(|e| !e.starts_with("NK_") && !e.starts_with("HOME=") && !e.starts_with("PATH="))
        .filter_map(|e| {
            e.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();

    // Parse memory from bytes to human string
    let memory = c
        .memory_limit
        .map(|bytes| {
            if bytes >= 1024 * 1024 * 1024 {
                format!("{}g", bytes / 1024 / 1024 / 1024)
            } else {
                format!("{}m", bytes / 1024 / 1024)
            }
        })
        .unwrap_or_else(|| "512m".to_string());

    // Detect volume_path from real mounts
    let volume_path: Option<String> = c
        .volume_mounts
        .iter()
        .find(|m| !m.starts_with("tmpfs:"))
        .and_then(|m| {
            // Format is usually "host_path:container_path" or just container_path
            m.split(':')
                .nth(1)
                .or(Some(m.as_str()))
                .map(|s| s.to_string())
        });

    let persistence = !c
        .volume_mounts
        .iter()
        .filter(|m| !m.starts_with("tmpfs:"))
        .collect::<Vec<_>>()
        .is_empty();

    DeploymentSpec {
        deployment: DeploymentMeta {
            name: c.name.clone(),
            image: normalize_image(&c.image),
            revision: 0, // init from running = revision 0
            command: if c.command.is_empty() {
                None
            } else {
                Some(c.command.join(" "))
            },
            created: c.created_at.clone(),
            updated: now,
        },
        resources: ResourceSpec {
            cpu: c.cpu_limit.map(|v| v as f32).unwrap_or(0.5),
            memory,
        },
        network: NetworkSpec {
            ports,
            ipv6: c.ipv6_enabled,
        },
        storage: StorageSpec {
            persistence,
            volume_path,
            volume_size: "1g".to_string(), // default — not available from inspect
        },
        env,
        placement: PlacementSpec {
            garage: None, // not available from inspect
            hardware: None,
        },
        lifecycle: LifecycleSpec {
            restart: c.labels.get("nordkraft.restart").cloned(),
            auto_stop: c.labels.get("nordkraft.auto_stop").map(|s| format!("{s}s")),
            auto_delete: c
                .labels
                .get("nordkraft.auto_delete")
                .map(|s| format!("{s}s")),
            max_lifetime: c
                .labels
                .get("nordkraft.max_lifetime")
                .map(|s| format!("{s}s")),
        },
        labels: c
            .labels
            .iter()
            .filter_map(|(k, v)| k.strip_prefix("user.").map(|k| (k.to_string(), v.clone())))
            .collect(),
    }
}

/// Normalize Docker image names for comparison.
/// containerd resolves short names: nginx:alpine → docker.io/library/nginx:alpine
/// This strips the prefix so both sides compare equal.
///
/// `registry://name:tag` shorthand is resolved to the private registry
/// address first: the live container stores what containerd pulled
/// (e.g. `172.21.1.3:5001/app:v9`) while the spec keeps the shorthand, and
/// the two must compare equal. If the registry is not initialised, or the
/// image is not a `registry://` reference, this step is a no-op.
pub(crate) fn normalize_image(image: &str) -> String {
    let resolved =
        super::registry::resolve_registry_image(image).unwrap_or_else(|_| image.to_string());
    resolved
        .strip_prefix("docker.io/library/")
        .or_else(|| resolved.strip_prefix("docker.io/"))
        .unwrap_or(&resolved)
        .to_string()
}

pub(crate) fn compute_diff(spec: &DeploymentSpec, live: &DeploymentSpec) -> Vec<SpecDiff> {
    let mut diffs = Vec::new();

    // Image (normalize docker.io/library/ prefix)
    let spec_image = normalize_image(&spec.deployment.image);
    let live_image = normalize_image(&live.deployment.image);
    diffs.push(SpecDiff {
        field: "image".to_string(),
        spec_value: spec.deployment.image.clone(),
        live_value: live.deployment.image.clone(),
        kind: if spec_image == live_image {
            DiffKind::Same
        } else {
            DiffKind::Changed
        },
    });

    // CPU
    let spec_cpu = format!("{}", spec.resources.cpu);
    let live_cpu = format!("{}", live.resources.cpu);
    diffs.push(SpecDiff {
        field: "cpu".to_string(),
        spec_value: spec_cpu.clone(),
        live_value: live_cpu.clone(),
        kind: if spec_cpu == live_cpu {
            DiffKind::Same
        } else {
            DiffKind::Changed
        },
    });

    // Memory
    diffs.push(SpecDiff {
        field: "memory".to_string(),
        spec_value: spec.resources.memory.clone(),
        live_value: live.resources.memory.clone(),
        kind: if spec.resources.memory == live.resources.memory {
            DiffKind::Same
        } else {
            DiffKind::Changed
        },
    });

    // Ports
    let spec_ports = format!("{:?}", spec.network.ports);
    let live_ports = format!("{:?}", live.network.ports);
    diffs.push(SpecDiff {
        field: "ports".to_string(),
        spec_value: spec_ports.clone(),
        live_value: live_ports.clone(),
        kind: if spec_ports == live_ports {
            DiffKind::Same
        } else {
            DiffKind::Changed
        },
    });

    // IPv6
    if spec.network.ipv6 != live.network.ipv6 {
        diffs.push(SpecDiff {
            field: "ipv6".to_string(),
            spec_value: spec.network.ipv6.to_string(),
            live_value: live.network.ipv6.to_string(),
            kind: DiffKind::Changed,
        });
    }

    // Command
    if spec.deployment.command != live.deployment.command {
        diffs.push(SpecDiff {
            field: "command".to_string(),
            spec_value: spec.deployment.command.clone().unwrap_or_default(),
            live_value: live.deployment.command.clone().unwrap_or_default(),
            kind: DiffKind::Changed,
        });
    }

    // Env vars — compare both directions
    let mut all_keys: Vec<String> = spec.env.keys().chain(live.env.keys()).cloned().collect();
    all_keys.sort();
    all_keys.dedup();

    for key in &all_keys {
        let in_spec = spec.env.get(key);
        let in_live = live.env.get(key);
        match (in_spec, in_live) {
            (Some(sv), Some(lv)) if sv == lv => {} // same, skip
            (Some(sv), Some(lv)) => {
                diffs.push(SpecDiff {
                    field: format!("env.{}", key),
                    spec_value: sv.clone(),
                    live_value: lv.clone(),
                    kind: DiffKind::Changed,
                });
            }
            (Some(sv), None) => {
                diffs.push(SpecDiff {
                    field: format!("env.{}", key),
                    spec_value: sv.clone(),
                    live_value: String::new(),
                    kind: DiffKind::Added,
                });
            }
            (None, Some(lv)) => {
                diffs.push(SpecDiff {
                    field: format!("env.{}", key),
                    spec_value: String::new(),
                    live_value: lv.clone(),
                    kind: DiffKind::Removed,
                });
            }
            (None, None) => {}
        }
    }

    diffs
}

/// Fetch live container state as a DeploymentSpec via the inspect endpoint.
/// Returns Ok(None) if the container exists in DB but has no live runtime state
/// (e.g. Failed deployment where container never started).
#[derive(Debug, Deserialize)]
pub(crate) struct StoredConfigResponse {
    pub(crate) config: StoredContainerConfig,
    #[serde(default)]
    pub(crate) revision: u32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StoredContainerConfig {
    pub(crate) container_name: String,
    pub(crate) image: String,
    #[serde(default)]
    pub(crate) ports: Vec<StoredPort>,
    pub(crate) command: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) env_vars: HashMap<String, String>,
    pub(crate) cpu_limit: f32,
    pub(crate) memory_limit: String,
    #[serde(default)]
    pub(crate) enable_persistence: bool,
    pub(crate) volume_path: Option<String>,
    #[serde(default = "default_volume_size")]
    pub(crate) volume_size: String,
    #[serde(default)]
    pub(crate) enable_ipv6: bool,
    #[serde(default)]
    pub(crate) lifecycle: LifecycleSpec,
    #[serde(default)]
    pub(crate) labels: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StoredPort {
    pub(crate) port: u16,
}

/// Fetch the deploy-time config the controller stored for this container and turn
/// it into a spec. Ok(None) means the container predates config tracking, so no
/// stored config exists and only the inspect path can rebuild it.
pub(crate) async fn fetch_stored_spec(
    name: &str,
) -> Result<Option<DeploymentSpec>, Box<dyn std::error::Error>> {
    let client = create_client()?;
    let url = format!("{}/containers/{}/config", *API_BASE_URL, name);
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        return Err(format!(
            "Failed to fetch stored config for '{}': HTTP {}",
            name,
            response.status()
        )
        .into());
    }

    let body = response.text().await?;

    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
        if val.get("error").is_some() {
            return Ok(None);
        }
    }

    let stored: StoredConfigResponse =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse stored config: {}", e))?;

    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let c = stored.config;

    Ok(Some(DeploymentSpec {
        deployment: DeploymentMeta {
            name: c.container_name,
            image: c.image,
            revision: stored.revision,
            command: c.command.map(|parts| parts.join(" ")),
            created: now.clone(),
            updated: now,
        },
        resources: ResourceSpec {
            cpu: c.cpu_limit,
            memory: c.memory_limit,
        },
        network: NetworkSpec {
            ports: c.ports.iter().map(|p| p.port).collect(),
            ipv6: c.enable_ipv6,
        },
        storage: StorageSpec {
            persistence: c.enable_persistence,
            volume_path: c.volume_path,
            volume_size: c.volume_size,
        },
        env: c.env_vars,
        // Placement is not part of the stored deploy config. Left unset rather
        // than guessed — an empty PlacementSpec is skipped when serialising.
        placement: PlacementSpec {
            garage: None,
            hardware: None,
        },
        lifecycle: c.lifecycle,
        labels: c.labels,
    }))
}

pub(crate) async fn fetch_live_spec(
    name: &str,
) -> Result<Option<LiveSpec>, Box<dyn std::error::Error>> {
    let client = create_client()?;
    let url = format!("{}/containers/{}", *API_BASE_URL, name);
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        let error: ApiError = response.json().await?;
        return Err(format!("Failed to inspect '{}': {}", name, error.error).into());
    }

    let body = response.text().await?;

    // Check if the response is an error JSON (container not running / never started)
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
        if val.get("error").is_some() {
            return Ok(None);
        }
    }

    let inspect: ContainerInspectResponse = serde_json::from_str(&body)
        .map_err(|e| format!("Failed to parse inspect response: {}", e))?;

    Ok(Some(LiveSpec {
        spec: spec_from_inspect(&inspect),
        unread_limits: unread_limits(&inspect),
    }))
}

/// A running container rebuilt as a spec, with the limits that had to be
/// defaulted because the runtime did not report them.
#[derive(Debug, Clone)]
pub(crate) struct LiveSpec {
    pub(crate) spec: DeploymentSpec,
    pub(crate) unread_limits: Vec<&'static str>,
}

/// The diff entries that are not `Same`, as the `--json` output and the MCP
/// server report them.
pub(crate) fn diff_changes_json(diffs: &[SpecDiff]) -> Vec<serde_json::Value> {
    diffs
        .iter()
        .filter(|d| d.kind != DiffKind::Same)
        .map(|d| {
            serde_json::json!({
                "field": d.field,
                "spec": d.spec_value,
                "live": d.live_value,
                "kind": format!("{:?}", d.kind).to_lowercase(),
            })
        })
        .collect()
}

// ============= EDITING SPECS IN PLACE =============

/// What `set_field` did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SpecSetOutcome {
    /// `+item`: appended to an array.
    Appended(String),
    /// `-item`: removed from an array.
    Removed(String),
    /// Plain value: replaced, or created when the field did not exist.
    Replaced { created: bool },
}

/// `spec-set`: set a dotted key in a `.nk` file, keeping comments and
/// formatting. `+item` appends to an array, `-item` removes from one;
/// anything else replaces. The result is validated as a `DeploymentSpec`
/// before it is written.
pub(crate) fn set_field(
    name: &str,
    key: &str,
    raw_value: &str,
) -> CoreResult<(PathBuf, SpecSetOutcome)> {
    let (path, mut doc) = load_doc(name)?;
    let (table, field) = parse_key_path(key)?;
    let strings_only = STRING_TABLES.contains(&table);

    // An empty table may be absent from the file; create it on first write.
    let table_tbl: &mut Table = doc
        .as_table_mut()
        .entry(table)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("Table '[{}]' missing from spec", table))?;

    let outcome = if strings_only {
        let created = !table_tbl.contains_key(field);
        table_tbl[field] = value(raw_value);
        SpecSetOutcome::Replaced { created }
    } else if let Some(stripped) = raw_value.strip_prefix('+') {
        let existing = table_tbl.entry(field).or_insert(value(Array::new()));
        let arr = existing
            .as_array_mut()
            .ok_or_else(|| format!("'{}' is not an array — cannot append", key))?;
        arr.push(coerce_array_element(stripped));
        SpecSetOutcome::Appended(stripped.to_string())
    } else if let Some(stripped) = raw_value.strip_prefix('-') {
        let existing = table_tbl
            .get_mut(field)
            .ok_or_else(|| format!("'{}' does not exist", key))?;
        let arr = existing
            .as_array_mut()
            .ok_or_else(|| format!("'{}' is not an array — cannot remove from", key))?;
        let before = arr.len();
        // Compare against each element's display form so typed arrays
        // (Vec<u16>, Vec<String>, Vec<bool>) all work with the same `-value` syntax.
        arr.retain(|v: &Value| {
            let as_str = v.as_str().map(|s| s.to_string());
            let as_int = v.as_integer().map(|i| i.to_string());
            let as_float = v.as_float().map(|f| f.to_string());
            let as_bool = v.as_bool().map(|b| b.to_string());
            let repr = as_str.or(as_int).or(as_float).or(as_bool);
            repr.as_deref() != Some(stripped)
        });
        if arr.len() == before {
            return Err(format!("'{}' not found in {}", stripped, key).into());
        }
        SpecSetOutcome::Removed(stripped.to_string())
    } else {
        let created = !table_tbl.contains_key(field);
        table_tbl[field] = coerce_value(raw_value);
        SpecSetOutcome::Replaced { created }
    };

    save_and_validate(&path, &doc)?;
    Ok((path, outcome))
}

/// `spec-unset`: remove a dotted key from a `.nk` file.
pub(crate) fn unset_field(name: &str, key: &str) -> CoreResult<PathBuf> {
    let (path, mut doc) = load_doc(name)?;
    let (table, field) = parse_key_path(key)?;

    let table_tbl: &mut Table = doc
        .as_table_mut()
        .get_mut(table)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| format!("Table '[{}]' missing", table))?;

    if table_tbl.remove(field).is_none() {
        return Err(format!("'{}' was not set", key).into());
    }

    save_and_validate(&path, &doc)?;
    Ok(path)
}

/// `spec-delete`: remove the `.nk` file. The container is not touched.
pub(crate) fn delete_spec(name: &str) -> CoreResult<PathBuf> {
    let path = nk_path(name);
    if !path.exists() {
        return Err(format!("No .nk spec found for '{}'", name).into());
    }
    std::fs::remove_file(&path)?;
    Ok(path)
}

/// Known top-level tables in a DeploymentSpec. Used to validate dotted keys
/// so a typo like `deployment.imag` doesn't silently create a dead field.
pub(crate) const KNOWN_TABLES: &[&str] = &[
    "deployment",
    "resources",
    "network",
    "storage",
    "placement",
    "lifecycle",
    "env",
    "labels",
];

/// Tables whose values are always strings (`env.PORT = 8080` must stay
/// `"8080"`), and where a leading `+`/`-` is part of the value, not an array
/// operation.
const STRING_TABLES: &[&str] = &["env", "labels"];

pub(crate) fn load_doc(name: &str) -> Result<(PathBuf, DocumentMut), Box<dyn Error>> {
    let path = nk_path(name);
    if !path.exists() {
        return Err(format!(
            "No .nk spec found for '{}'. Run 'nordkraft init {}' first.",
            name, name
        )
        .into());
    }
    let contents = std::fs::read_to_string(&path)?;
    let doc: DocumentMut = contents
        .parse()
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
    Ok((path, doc))
}

pub(crate) fn save_and_validate(path: &PathBuf, doc: &DocumentMut) -> Result<(), Box<dyn Error>> {
    let serialized = doc.to_string();
    // Round-trip through DeploymentSpec to catch type errors early.
    if let Err(e) = toml::from_str::<DeploymentSpec>(&serialized) {
        return Err(format!(
            "Refusing to save: result would be an invalid spec.\n  → {}",
            e
        )
        .into());
    }
    std::fs::write(path, serialized)?;
    Ok(())
}

/// Parse a dotted key path like "resources.cpu" or "deployment.image" into
/// (table, field). Single-segment keys are rejected — every field lives under
/// a table in our schema.
pub(crate) fn parse_key_path(key: &str) -> Result<(&str, &str), Box<dyn Error>> {
    let parts: Vec<&str> = key.splitn(2, '.').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Key must be dotted (e.g. 'resources.cpu', 'deployment.image'). Got: '{}'",
            key
        )
        .into());
    }
    let (table, field) = (parts[0], parts[1]);
    if !KNOWN_TABLES.contains(&table) {
        return Err(format!(
            "Unknown table '{}'. Known tables: {}",
            table,
            KNOWN_TABLES.join(", ")
        )
        .into());
    }
    Ok((table, field))
}

/// Coerce a CLI string into the right TOML type by trying int → float → bool → string.
pub(crate) fn coerce_value(raw: &str) -> Item {
    if let Ok(i) = raw.parse::<i64>() {
        return value(i);
    }
    if let Ok(f) = raw.parse::<f64>() {
        return value(f);
    }
    match raw.to_ascii_lowercase().as_str() {
        "true" => return value(true),
        "false" => return value(false),
        _ => {}
    }
    value(raw)
}

/// Same coercion but returns a `Value` suitable for pushing into an Array.
pub(crate) fn coerce_array_element(raw: &str) -> Value {
    if let Ok(i) = raw.parse::<i64>() {
        return Value::from(i);
    }
    if let Ok(f) = raw.parse::<f64>() {
        return Value::from(f);
    }
    match raw.to_ascii_lowercase().as_str() {
        "true" => return Value::from(true),
        "false" => return Value::from(false),
        _ => {}
    }
    Value::from(raw)
}

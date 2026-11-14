//! `nordkraft mcp`: an MCP server on stdio for agents (Claude Code, Claude
//! Desktop, Cursor). Every tool wraps the `core` layer, so an agent and a
//! human at the terminal run the same code and get the same answers and the
//! same errors. The agent reaches the controller the way the CLI does — over
//! the user's WireGuard tunnel — so there is no API key and no new auth
//! surface: if the tunnel is down, every tool fails the same way.
//!
//! Stdout is the protocol channel. Nothing in this module, and nothing it
//! calls, may print to it.

use std::collections::HashMap;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::core::spec::{
    compute_diff, diff_changes_json, fetch_live_spec, fetch_stored_spec, list_deployment_specs,
    load_deployment_spec, nk_path, save_deployment_spec, set_field, unset_field, SpecSetOutcome,
};
use crate::core::{aliases, containers, deploy, ingress, usage};

type ToolResult = Result<CallToolResult, McpError>;

/// A successful call: structured JSON for the agent, with a one-line summary
/// in front of the same JSON as text for clients that only show text.
fn ok(summary: impl Into<String>, data: serde_json::Value) -> ToolResult {
    let text = format!(
        "{}\n\n{}",
        summary.into(),
        serde_json::to_string_pretty(&data).unwrap_or_default()
    );
    let mut result = CallToolResult::structured(data);
    result.content = vec![ContentBlock::text(text)];
    Ok(result)
}

/// A tool-level error: the message the CLI would have shown, as an MCP tool
/// error (never a protocol error, so the agent gets to read it).
fn fail(message: impl Into<String>) -> ToolResult {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        message.into(),
    )]))
}

/// The message for an error from core. A connection failure is the tunnel.
fn error_text(e: &(dyn std::error::Error + 'static)) -> String {
    if let Some(re) = e.downcast_ref::<reqwest::Error>() {
        if re.is_connect() || re.is_timeout() {
            return "Cannot reach the API. Is WireGuard connected? (nordkraft connect)".to_string();
        }
    }
    e.to_string()
}

fn fail_with(e: Box<dyn std::error::Error>) -> ToolResult {
    fail(error_text(&*e))
}

/// Every known alias, as `name → alias`, for display next to container names.
fn reverse_aliases() -> HashMap<String, String> {
    aliases::load_aliases()
        .into_iter()
        .map(|(alias, name)| (name, alias))
        .collect()
}

fn with_alias(name: &str, reverse: &HashMap<String, String>) -> String {
    match reverse.get(name) {
        Some(alias) => format!("{name} [{alias}]"),
        None => name.to_string(),
    }
}

// ============= PARAMETERS =============

#[derive(Deserialize, JsonSchema)]
struct NameParams {
    /// Container name (app-…) or alias
    name: String,
}

fn default_lines() -> usize {
    100
}

#[derive(Deserialize, JsonSchema)]
struct LogsParams {
    /// Container name (app-…) or alias
    name: String,
    /// Number of lines from the end of the log (default 100)
    #[serde(default = "default_lines")]
    lines: usize,
}

fn default_limit() -> usize {
    20
}

#[derive(Deserialize, JsonSchema)]
struct EventsParams {
    /// Only events for this container name or alias
    #[serde(default)]
    container: Option<String>,
    /// Number of events, newest first (default 20)
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize, JsonSchema)]
struct ImageParams {
    /// Container image, e.g. nginx:alpine or ghcr.io/user/app:v1
    image: String,
}

#[derive(Deserialize, JsonSchema)]
struct SpecSetParams {
    /// Container name or alias; the spec is ~/.nordkraft/deployments/<name>.nk
    name: String,
    /// Dotted key: deployment.image, deployment.command, resources.cpu,
    /// resources.memory, network.ports, network.ipv6, storage.persistence,
    /// storage.volume_path, storage.volume_size, placement.garage,
    /// lifecycle.restart|auto_stop|auto_delete|max_lifetime, env.<KEY>,
    /// labels.<KEY>
    key: String,
    /// New value. For arrays (network.ports), "+item" appends and "-item"
    /// removes. Numbers and booleans are typed automatically, except under
    /// env and labels where every value is a string.
    value: String,
}

#[derive(Deserialize, JsonSchema)]
struct SpecUnsetParams {
    /// Container name or alias
    name: String,
    /// Dotted key to remove, e.g. network.ipv6 or env.DEBUG
    key: String,
}

#[derive(Deserialize, JsonSchema)]
struct InitSpecParams {
    /// Container name or alias
    name: String,
    /// Rebuild from the config stored at deploy time instead of from the
    /// running container (exact values, keeps the server's revision)
    #[serde(default)]
    from_server: bool,
    /// Replace an existing spec file. Default false: refuse if one exists.
    #[serde(default)]
    overwrite: bool,
}

// ============= SERVER =============

#[derive(Clone, Default)]
pub(crate) struct NordkraftMcp;

#[tool_router]
impl NordkraftMcp {
    // ----- Tier 1: read-only -----

    #[tool(
        description = "List the user's containers with status, image, IPs, ports and power draw, plus the local alias table (alias → container name). Aliases and names are interchangeable in every other tool.",
        annotations(title = "List containers", read_only_hint = true)
    )]
    async fn list_containers(&self) -> ToolResult {
        match containers::list(&[]).await {
            Ok(data) => {
                let reverse = reverse_aliases();
                let items: Vec<String> = data
                    .containers
                    .iter()
                    .map(|c| format!("{} ({})", with_alias(&c.name, &reverse), c.status))
                    .collect();
                ok(
                    format!("{} container(s): {}", items.len(), items.join(", ")),
                    json!({
                        "containers": data.containers,
                        "aliases": aliases::load_aliases(),
                    }),
                )
            }
            Err(e) => fail_with(e),
        }
    }

    #[tool(
        description = "Full details of one container: image and digest, status, uptime fields, node, runtime, IPs, ports with access URLs, resource limits, volumes, environment and labels.",
        annotations(title = "Inspect container", read_only_hint = true)
    )]
    async fn inspect_container(&self, Parameters(p): Parameters<NameParams>) -> ToolResult {
        let name = aliases::resolve_alias(&p.name);
        match containers::inspect(&name).await {
            Ok(containers::Inspect::Parsed(c)) => {
                let ports: Vec<String> = c
                    .ports
                    .iter()
                    .filter_map(|p| p["port"].as_u64().map(|v| v.to_string()))
                    .collect();
                let summary = format!(
                    "{} is {} on {} ({}), ports: {}",
                    c.name,
                    c.status,
                    c.node_id,
                    c.image,
                    if ports.is_empty() {
                        "none — ingress will not work".to_string()
                    } else {
                        ports.join(", ")
                    }
                );
                ok(summary, serde_json::to_value(&*c).unwrap_or_default())
            }
            Ok(containers::Inspect::Raw(body)) => ok(
                format!("{name}: the controller returned a plain reply"),
                json!({"container": name, "raw": body}),
            ),
            Err(e) => fail_with(e),
        }
    }

    #[tool(
        description = "The last lines of a container's log. A container that is still starting has no log yet; the result then carries its status and a message instead.",
        annotations(title = "Get logs", read_only_hint = true)
    )]
    async fn get_logs(&self, Parameters(p): Parameters<LogsParams>) -> ToolResult {
        let name = aliases::resolve_alias(&p.name);
        match containers::logs(&name, p.lines).await {
            Ok(containers::Logs::Ready(logs)) => ok(
                format!(
                    "{} line(s) from {}{}",
                    logs.logs.lines().count(),
                    name,
                    logs.source
                        .as_deref()
                        .map(|s| format!(" [{s}]"))
                        .unwrap_or_default()
                ),
                json!({"container": name, "logs": logs.logs, "source": logs.source}),
            ),
            Ok(containers::Logs::NotReady { status, raw }) => {
                let message = match status.as_deref() {
                    Some("running") => "Container is running but logs are not available yet. Try again in a moment.",
                    Some("created") | Some("starting") => "Container is still starting up.",
                    Some(_) => "Container is not running.",
                    None => "Could not parse logs response; the container may still be starting.",
                };
                ok(
                    format!("{name}: no logs yet — {message}"),
                    json!({"container": name, "status": status, "logs": null, "message": message, "raw": raw}),
                )
            }
            Err(e) => fail_with(e),
        }
    }

    #[tool(
        description = "Plan usage: the plan's limits (cpu, memory_mb, storage_mb), what is allocated right now, and ratios 0..1. Check before a deploy that adds resources.",
        annotations(title = "Get usage", read_only_hint = true)
    )]
    async fn get_usage(&self) -> ToolResult {
        match usage::usage().await {
            Ok(data) => {
                let plan = data["plan"]["name"].as_str().unwrap_or("?");
                let cpu = data["ratios"]["cpu"].as_f64().unwrap_or(0.0);
                let mem = data["ratios"]["memory"].as_f64().unwrap_or(0.0);
                let disk = data["ratios"]["disk"].as_f64().unwrap_or(0.0);
                ok(
                    format!(
                        "Plan {plan}: cpu {:.0}%, memory {:.0}%, disk {:.0}% of the limits in use",
                        cpu * 100.0,
                        mem * 100.0,
                        disk * 100.0
                    ),
                    data,
                )
            }
            Err(e) => fail_with(e),
        }
    }

    #[tool(
        description = "Deploy lifecycle events (pulling, pulled, created, starting, running, upgrading, failed), newest first. The place to look when a container is not running: the failed event carries the reason.",
        annotations(title = "Get events", read_only_hint = true)
    )]
    async fn get_events(&self, Parameters(p): Parameters<EventsParams>) -> ToolResult {
        let container = p.container.as_deref().map(aliases::resolve_alias);
        match usage::events(container.as_deref(), p.limit).await {
            Ok(data) => {
                let n = data["events"].as_array().map(|a| a.len()).unwrap_or(0);
                ok(
                    match &container {
                        Some(c) => format!("{n} event(s) for {c}"),
                        None => format!("{n} event(s)"),
                    },
                    data,
                )
            }
            Err(e) => fail_with(e),
        }
    }

    #[tool(
        description = "All HTTPS ingress routes: subdomain, public URL, container, target port, mode and whether the route is active.",
        annotations(title = "List ingress routes", read_only_hint = true)
    )]
    async fn list_ingress(&self) -> ToolResult {
        match ingress::list().await {
            Ok(data) => {
                let items: Vec<String> = data
                    .routes
                    .iter()
                    .map(|r| format!("{} → {}", r.url, r.container_id))
                    .collect();
                ok(
                    format!("{} route(s): {}", items.len(), items.join(", ")),
                    serde_json::to_value(&data).unwrap_or_default(),
                )
            }
            Err(e) => fail_with(e),
        }
    }

    #[tool(
        description = "Ingress status for one container: enabled, URL, target port, TLS certificate status.",
        annotations(title = "Ingress status", read_only_hint = true)
    )]
    async fn ingress_status(&self, Parameters(p): Parameters<NameParams>) -> ToolResult {
        let name = aliases::resolve_alias(&p.name);
        match ingress::status(&name).await {
            Ok(s) => ok(
                if s.enabled {
                    format!(
                        "{name}: ingress enabled at {}",
                        s.url.as_deref().unwrap_or("?")
                    )
                } else {
                    format!("{name}: no ingress")
                },
                serde_json::to_value(&s).unwrap_or_default(),
            ),
            Err(e) => fail_with(e),
        }
    }

    #[tool(
        description = "The port a well-known image usually listens on (nginx/caddy 80, node/rails 3000, python frameworks 8000, java/go 8080). Databases return null: they expose no HTTP port. Use it to pick the ports argument for deploy when the user did not say.",
        annotations(title = "Suggest port", read_only_hint = true)
    )]
    fn suggest_port(&self, Parameters(p): Parameters<ImageParams>) -> ToolResult {
        let port = deploy::suggest_port(&p.image);
        ok(
            match port {
                Some(port) => format!("{}: usually port {port}", p.image),
                None => format!("{}: no HTTP port (database image)", p.image),
            },
            json!({"image": p.image, "port": port}),
        )
    }

    // ----- Tier 2: .nk specs, where the agent plans -----

    #[tool(
        description = "The saved .nk deployment specs on this machine: name, image, revision, last update and alias. Every deploy saves one; it is the desired state that upgrade applies.",
        annotations(title = "List specs", read_only_hint = true)
    )]
    fn list_specs(&self) -> ToolResult {
        let items = spec_summaries();
        let names: Vec<String> = items
            .iter()
            .map(|i| {
                i["alias"]
                    .as_str()
                    .or_else(|| i["name"].as_str())
                    .unwrap_or("?")
                    .to_string()
            })
            .collect();
        ok(
            format!("{} spec(s): {}", items.len(), names.join(", ")),
            json!({"specs": items}),
        )
    }

    #[tool(
        description = "Read a container's .nk spec as JSON: deployment (name, image, revision, command), resources (cpu, memory), network (ports, ipv6), storage, env, placement, lifecycle, labels.",
        annotations(title = "Read spec", read_only_hint = true)
    )]
    fn read_spec(&self, Parameters(p): Parameters<NameParams>) -> ToolResult {
        let name = aliases::resolve_alias(&p.name);
        match load_deployment_spec(&name) {
            Some(spec) => ok(
                format!(
                    "{}: {} r{} — {} cpu, {}, ports {:?}",
                    name,
                    spec.deployment.image,
                    spec.deployment.revision,
                    spec.resources.cpu,
                    spec.resources.memory,
                    spec.network.ports
                ),
                json!({"path": nk_path(&name), "spec": spec}),
            ),
            None => fail(no_spec(&name)),
        }
    }

    #[tool(
        description = "Set one field in a container's .nk spec (local file only; nothing changes on the platform until upgrade). Dotted key, e.g. resources.memory = 1g, network.ports +443, network.ports -8080, env.DEBUG = true. Returns the updated spec. Follow with diff_spec, then upgrade.",
        annotations(
            title = "Set spec field",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    fn spec_set(&self, Parameters(p): Parameters<SpecSetParams>) -> ToolResult {
        let name = aliases::resolve_alias(&p.name);
        match set_field(&name, &p.key, &p.value) {
            Ok((path, outcome)) => {
                let what = match &outcome {
                    SpecSetOutcome::Appended(item) => format!("{} += {}", p.key, item),
                    SpecSetOutcome::Removed(item) => format!("{} -= {}", p.key, item),
                    SpecSetOutcome::Replaced { created: true } => {
                        format!("{} = {} (new field)", p.key, p.value)
                    }
                    SpecSetOutcome::Replaced { created: false } => {
                        format!("{} = {}", p.key, p.value)
                    }
                };
                ok(
                    format!("{name}: {what}. Run diff_spec, then upgrade to apply."),
                    json!({
                        "path": path,
                        "change": what,
                        "spec": load_deployment_spec(&name),
                    }),
                )
            }
            Err(e) => fail_with(e),
        }
    }

    #[tool(
        description = "Remove one field from a container's .nk spec (local file only). Returns the updated spec.",
        annotations(
            title = "Unset spec field",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    fn spec_unset(&self, Parameters(p): Parameters<SpecUnsetParams>) -> ToolResult {
        let name = aliases::resolve_alias(&p.name);
        match unset_field(&name, &p.key) {
            Ok(path) => ok(
                format!("{name}: removed {}", p.key),
                json!({"path": path, "spec": load_deployment_spec(&name)}),
            ),
            Err(e) => fail_with(e),
        }
    }

    #[tool(
        description = "Compare a container's .nk spec with the running container. Returns the fields that differ ({field, spec, live, kind: changed|added|removed}); an empty list means the spec is applied. A container with no live state (never started, failed) reports live_state=false: upgrade will then redeploy it from the spec.",
        annotations(title = "Diff spec", read_only_hint = true)
    )]
    async fn diff_spec(&self, Parameters(p): Parameters<NameParams>) -> ToolResult {
        let name = aliases::resolve_alias(&p.name);
        let Some(local) = load_deployment_spec(&name) else {
            return fail(no_spec(&name));
        };
        match fetch_live_spec(&name).await {
            Ok(Some(live)) => {
                let diffs = compute_diff(&local, &live.spec);
                let changes = diff_changes_json(&diffs);
                let warnings = unread_warnings(&live.unread_limits);
                let summary = if changes.is_empty() {
                    format!("{name}: spec matches the live container")
                } else {
                    format!(
                        "{name}: {} change(s) pending — {}",
                        changes.len(),
                        changes
                            .iter()
                            .map(|c| c["field"].as_str().unwrap_or("?").to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                ok(
                    summary,
                    json!({"container": name, "live_state": true, "changes": changes, "warnings": warnings}),
                )
            }
            Ok(None) => ok(
                format!("{name}: no live state (never started or failed); upgrade will redeploy from the spec"),
                json!({"container": name, "live_state": false, "changes": [], "warnings": []}),
            ),
            Err(e) => fail_with(e),
        }
    }

    #[tool(
        description = "Create a container's .nk spec from the platform when none exists locally: from the running container (default) or, with from_server, from the config stored at deploy time. Needed before spec_set/upgrade on a container that was not deployed from this machine.",
        annotations(title = "Init spec", read_only_hint = false, destructive_hint = false)
    )]
    async fn init_spec(&self, Parameters(p): Parameters<InitSpecParams>) -> ToolResult {
        let name = aliases::resolve_alias(&p.name);
        if let Some(existing) = load_deployment_spec(&name) {
            if !p.overwrite {
                return fail(format!(
                    "{name}.nk already exists (revision {}). Pass overwrite=true to replace it, or use read_spec.",
                    existing.deployment.revision
                ));
            }
        }
        let (spec, source, warnings) = if p.from_server {
            match fetch_stored_spec(&name).await {
                Ok(Some(spec)) => (spec, "server", Vec::new()),
                Ok(None) => {
                    return fail(format!(
                        "No stored config for '{name}' — deployed before config tracking. Call init_spec without from_server to rebuild from the running container."
                    ))
                }
                Err(e) => return fail_with(e),
            }
        } else {
            match fetch_live_spec(&name).await {
                Ok(Some(live)) => (
                    live.spec,
                    "running container",
                    unread_warnings(&live.unread_limits),
                ),
                Ok(None) => {
                    return fail(format!(
                        "Container '{name}' has no live state — cannot generate a spec from a failed deployment."
                    ))
                }
                Err(e) => return fail_with(e),
            }
        };
        let revision = spec.deployment.revision;
        if let Err(e) = save_deployment_spec(&spec) {
            return fail_with(e);
        }
        // The file is named after the spec's own name (the platform's), which
        // is what the caller asked for unless they passed an alias.
        let path = nk_path(&spec.deployment.name);
        ok(
            format!(
                "{name}: wrote {} (revision {revision}, from the {source})",
                path.display()
            ),
            json!({"path": path, "revision": revision, "source": source, "warnings": warnings, "spec": spec}),
        )
    }
}

fn no_spec(name: &str) -> String {
    format!(
        "No .nk spec found for '{name}'. Call init_spec to create one from the platform, or deploy/deploy_from_spec which save one."
    )
}

fn unread_warnings(unread: &[&str]) -> Vec<String> {
    if unread.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "Could not read {} from the running container; the live side shows defaults (0.5 cpu / 512m). Verify before trusting those fields.",
            unread.join(" and ")
        )]
    }
}

/// The `--json specs` rows: name, image, revision, updated and alias.
pub(crate) fn spec_summaries() -> Vec<serde_json::Value> {
    let reverse = reverse_aliases();
    list_deployment_specs()
        .iter()
        .filter_map(|name| {
            load_deployment_spec(name).map(|s| {
                let mut item = json!({
                    "name": s.deployment.name,
                    "image": s.deployment.image,
                    "revision": s.deployment.revision,
                    "updated": s.deployment.updated,
                });
                if let Some(alias) = reverse.get(&s.deployment.name) {
                    item["alias"] = json!(alias);
                }
                item
            })
        })
        .collect()
}

const INSTRUCTIONS: &str = "Nordkraft Garage Cloud: containers on the user's own hosting, reached over their WireGuard tunnel. \
Names look like app-<uuid> and are assigned by the platform; aliases are short local names. Both work everywhere. \
Read-only tools (list_containers, inspect_container, get_logs, get_usage, get_events, list_ingress, ingress_status, suggest_port, list_specs, read_spec, diff_spec) may be called freely. \
Every deploy saves a .nk spec; to change a running container, call spec_set, then diff_spec, then upgrade. \
Mutating tools take dry_run=true to show what would be sent without sending it. \
If a tool fails with 'Cannot reach the API', the user's WireGuard tunnel is down: ask them to run `nordkraft connect`.";

#[tool_handler]
impl ServerHandler for NordkraftMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("nordkraft", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serve MCP on stdin/stdout until the client goes away.
pub(crate) async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Stdout is the wire: no colour codes, no prompts, ever.
    colored::control::set_override(false);
    let service = NordkraftMcp.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

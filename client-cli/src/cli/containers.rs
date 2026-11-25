//! `nordkraft list / inspect / logs / start / stop / rm` — terminal output.

use colored::*;
use console::Term;
use dialoguer::{theme::ColorfulTheme, Select};

use crate::core::aliases::{load_aliases, resolve_alias};
use crate::core::api::*;
use crate::core::containers::{self, Inspect, Logs};
use crate::spinner::{self, Spinner};

pub(crate) async fn handle_container_list(
    labels: &[String],
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let sp = if json_output {
        None
    } else {
        Some(Spinner::start(spinner::LIST_MESSAGES))
    };

    let result = containers::list(labels).await;
    if let Some(s) = sp {
        s.stop();
    }
    let data = result?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&data.containers)?);
        return Ok(());
    }

    if data.containers.is_empty() {
        println!("{}", "No containers found.".yellow());
        println!("   Deploy one: nordkraft deploy nginx:alpine");
        return Ok(());
    }

    // Load aliases for display
    let aliases = load_aliases();
    let reverse_aliases: std::collections::HashMap<&String, &String> = aliases
        .iter()
        .map(|(alias, container_name)| (container_name, alias))
        .collect();

    println!(
        "{}",
        format!("🐳 {} container(s):", data.containers.len())
            .green()
            .bold()
    );
    println!();

    for c in &data.containers {
        let status_lower = c.status.to_lowercase();
        let (status_colored, failure_reason) =
            if status_lower.contains("up") || status_lower == "running" {
                ("Up".green().bold(), None)
            } else if status_lower == "deploying" {
                ("Deploying".yellow().bold(), None)
            } else if status_lower.starts_with("failed") {
                let reason = if status_lower.starts_with("failed: ") {
                    Some(c.status[8..].to_string())
                } else {
                    c.status_message.clone()
                };
                ("Failed".red().bold(), reason)
            } else if status_lower.contains("exited") || status_lower.contains("stopped") {
                (c.status.red(), None)
            } else {
                (c.status.yellow(), None)
            };

        // Show alias as primary name if available
        if let Some(alias) = reverse_aliases.get(&c.name) {
            println!(
                "  {} {} {}",
                "NAME:".cyan().bold(),
                alias.white().bold(),
                format!("({})", c.name).dimmed()
            );
        } else {
            println!("  {} {}", "NAME:".cyan().bold(), c.name.white().bold());
        }
        println!("    {} {}", "Image:".dimmed(), c.image);
        println!("    {} {}", "Status:".dimmed(), status_colored);
        if let Some(w) = c.watts {
            println!("    {} {}", "Power:".dimmed(), format!("{w:.1} W").yellow());
        }

        // Show failure reason
        if let Some(reason) = failure_reason {
            let truncated: String = reason.chars().take(120).collect();
            println!("    {} {}", "Reason:".dimmed(), truncated.red());
            println!(
                "    {} {}",
                "💡".dimmed(),
                "Run 'nordkraft events' for full details".dimmed()
            );
        }

        if let Some(ip) = &c.container_ip {
            println!("    {} {}", "IPv4:".dimmed(), ip);
        }
        if let Some(ipv6) = &c.ipv6_address {
            println!("    {} {}", "IPv6:".dimmed(), ipv6.cyan());
        }
        if !c.ports.is_empty() {
            let ports_str: Vec<String> = c
                .ports
                .iter()
                .map(|p| match p {
                    PortInfo::Simple(s) => s.clone(),
                    PortInfo::Detailed { port, protocol, .. } => {
                        format!("{}/{}", port, protocol.as_deref().unwrap_or("tcp"))
                    }
                })
                .collect();
            println!("    {} {}", "Ports:".dimmed(), ports_str.join(", "));
        }
        println!("    {} {}", "Created:".dimmed(), c.created_at.dimmed());
        println!();
    }

    Ok(())
}

pub(crate) async fn handle_container_start(
    container: &str,
    wait: bool,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let container = resolve_alias(container);

    if !json_output {
        println!("{}", format!("▶️  Starting {}...", container).cyan());
    }

    let result = containers::start(&container, wait).await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        match result.status.as_deref() {
            Some("running") => println!("{} Container running", "✅".green().bold()),
            Some("timeout") => println!(
                "{} Start sent, but the container did not report running in time",
                "⚠️".yellow().bold()
            ),
            _ => println!("{} Container started", "✅".green().bold()),
        }
    }

    Ok(())
}

pub(crate) async fn handle_container_stop(
    container: &str,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let container = resolve_alias(container);

    if !json_output {
        println!("{}", format!("⏸️  Stopping {}...", container).cyan());
    }

    let result = containers::stop(&container).await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("{} Container stopped", "✅".green().bold());
    }

    Ok(())
}

pub(crate) async fn handle_container_remove(
    container: &str,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    handle_container_remove_opts(container, false, json_output).await
}

pub(crate) async fn handle_container_remove_opts(
    container: &str,
    force: bool,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let container = resolve_alias(container);

    if !json_output {
        println!("{}", format!("🗑️  Removing {}...", container).cyan());
    }

    let result = containers::remove(&container, force).await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("{} Container removed", "✅".green().bold());
    }

    Ok(())
}

pub(crate) async fn handle_container_logs(
    container: &str,
    lines: usize,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Resolve alias to full name
    let container = resolve_alias(container);

    let sp = if json_output {
        None
    } else {
        Some(Spinner::start(spinner::LOGS_MESSAGES))
    };

    // A body that is not a logs document means the container may not be ready yet
    let result = containers::logs(&container, lines).await;
    if let Some(s) = sp {
        s.stop();
    }

    match result? {
        Logs::Ready(logs) => {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "container": container,
                        "logs": logs.logs,
                        "source": logs.source
                    }))?
                );
            } else {
                if let Some(source) = logs.source {
                    println!(
                        "{}",
                        format!("📜 Logs from {} [{}]:", container, source)
                            .cyan()
                            .dimmed()
                    );
                }
                println!("{}", logs.logs);
            }
        }
        Logs::NotReady { status, raw: body } => {
            // Could not parse logs — the container status gives a useful message
            match status.as_deref() {
                Some("running") => {
                    // Running but logs not ready yet (race condition right after deploy)
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({"container": container, "status": "running", "logs": null, "message": "Container is running but logs are not available yet. Try again in a moment."})
                        );
                    } else {
                        println!(
                            "{}",
                            "⏳ Container is running but logs aren't ready yet.".yellow()
                        );
                        println!("{}", "   Try again in a moment:".dimmed());
                        println!("   nordkraft logs {}", container);
                    }
                }
                Some(s) if s == "created" || s == "starting" => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({"container": container, "status": s, "message": "Container is still starting up."})
                        );
                    } else {
                        println!(
                            "{}",
                            "⏳ Container is still starting up — no logs yet.".yellow()
                        );
                        println!("{}", "   Give it a few seconds:".dimmed());
                        println!("   nordkraft logs {}", container);
                    }
                }
                Some(s) => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({"container": container, "status": s, "message": "Container is not running."})
                        );
                    } else {
                        println!(
                            "{}",
                            format!("⚠️  Container status is '{}' — logs unavailable.", s).yellow()
                        );
                    }
                }
                None => {
                    // Could not fetch status either — surface raw body for debugging
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({"container": container, "error": "Could not parse logs response", "raw": body})
                        );
                    } else {
                        println!(
                            "{}",
                            "⚠️  Could not retrieve logs — container may still be starting."
                                .yellow()
                        );
                        println!("{}", "   Try again in a moment:".dimmed());
                        println!("   nordkraft logs {}", container);
                    }
                }
            }
        }
    }

    Ok(())
}

pub(crate) async fn handle_container_inspect(
    container: &str,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let container = resolve_alias(container);

    let sp = if json_output {
        None
    } else {
        Some(Spinner::start(spinner::INSPECT_MESSAGES))
    };

    let result = containers::inspect(&container).await;
    if let Some(s) = sp {
        s.stop();
    }

    let c = match result? {
        Inspect::Parsed(c) => *c,
        Inspect::Raw(body) => {
            // Fallback: if server returns old-style simple data (e.g. error or pre-update node)
            println!("{}", body);
            return Ok(());
        }
    };

    {
        if json_output {
            println!("{}", serde_json::to_string_pretty(&c)?);
            return Ok(());
        }

        // Status color
        let status_colored = match c.status.as_str() {
            "running" => c.status.green().bold(),
            "stopped" | "exited" => c.status.red(),
            _ => c.status.yellow(),
        };

        // Uptime from started_at — parse ISO8601, correct epoch calculation
        let uptime = c.started_at.as_ref().and_then(|s| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs() as i64;
            let s = s.trim_end_matches('Z');
            let parts: Vec<&str> = s.splitn(2, 'T').collect();
            if parts.len() != 2 {
                return None;
            }
            let date: Vec<u32> = parts[0].split('-').filter_map(|x| x.parse().ok()).collect();
            let time_str = parts[1].split('.').next().unwrap_or(parts[1]);
            let time: Vec<u32> = time_str.split(':').filter_map(|x| x.parse().ok()).collect();
            if date.len() < 3 || time.len() < 3 {
                return None;
            }

            let year = date[0] as i64;
            let month = date[1] as i64;
            let day = date[2] as i64;

            // Days per month (non-leap year — close enough for uptime display)
            let days_per_month: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

            // Days from epoch to start of year
            let y = year - 1970;
            let leap_days = (y / 4) - (y / 100) + (y / 400);
            let mut days = y * 365 + leap_days;

            // Add days for each completed month this year
            for month_days in days_per_month.iter().take((month - 1) as usize) {
                days += *month_days;
            }
            days += day - 1;

            let epoch_secs =
                days * 86400 + time[0] as i64 * 3600 + time[1] as i64 * 60 + time[2] as i64;

            let elapsed = now - epoch_secs;
            if elapsed < 0 {
                return None;
            }
            if elapsed < 60 {
                Some(format!("{}s", elapsed))
            } else if elapsed < 3600 {
                Some(format!("{}m {}s", elapsed / 60, elapsed % 60))
            } else if elapsed < 86400 {
                Some(format!("{}h {}m", elapsed / 3600, (elapsed % 3600) / 60))
            } else {
                Some(format!(
                    "{}d {}h",
                    elapsed / 86400,
                    (elapsed % 86400) / 3600
                ))
            }
        });

        // Pull interesting OCI/image labels
        let restart_policy = c
            .labels
            .get("containerd.io/restart.policy")
            .map(String::as_str);
        let oci_version = c.labels.get("org.opencontainers.image.version");
        let oci_source = c.labels.get("org.opencontainers.image.source");
        let oci_revision = c.labels.get("org.opencontainers.image.revision");
        let tenant_id = c.labels.get("tenant_id");

        println!("{}", format!("🔍 {}", c.name).cyan().bold());
        println!();

        // ── Identity ──────────────────────────────────────────────
        println!(
            "   {} {}",
            "ID:".cyan(),
            c.container_id.as_deref().unwrap_or("n/a").dimmed()
        );
        println!("   {} {}", "Image:".cyan(), c.image);
        if let Some(ver) = oci_version {
            println!("   {} {}", "Version:".cyan(), ver.dimmed());
        }
        println!("   {} {}", "Status:".cyan(), status_colored);
        if let Some(up) = &uptime {
            println!("   {} {}", "Uptime:".cyan(), up.green());
        }
        println!("   {} {}", "Node:".cyan(), c.node_id);
        println!("   {} {}", "Runtime:".cyan(), c.runtime);
        if let Some(policy) = restart_policy {
            println!("   {} {}", "Restart:".cyan(), policy.dimmed());
        }
        if let Some(tid) = tenant_id {
            println!("   {} {}", "Tenant:".cyan(), tid.dimmed());
        }
        println!();

        // ── Network ───────────────────────────────────────────────
        println!("   {}", "Network:".cyan().bold());
        if let Some(ip) = &c.container_ip {
            println!("     {} {}", "IPv4:".cyan(), ip.white().bold());
        }
        if let Some(ipv6) = &c.ipv6_address {
            println!("     {} {}", "IPv6:".cyan(), ipv6.white().bold());
        }
        if let Some(h) = &c.hostname {
            println!("     {} {}", "Hostname:".cyan(), h.dimmed());
        }
        if !c.ports.is_empty() {
            for p in &c.ports {
                let port_num = p["port"].as_u64().unwrap_or(0);
                let proto = p["protocol"].as_str().unwrap_or("tcp");
                let url = p["access_url"].as_str().unwrap_or("");
                println!(
                    "     {} {}/{} → {}",
                    "Port:".cyan(),
                    port_num,
                    proto,
                    url.white()
                );
                if let Some(v6url) = p["ipv6_url"].as_str() {
                    println!("     {}      {}", "".dimmed(), v6url.dimmed());
                }
            }
        } else {
            println!(
                "     {} No ports exposed — ingress will not work",
                "⚠".yellow()
            );
            println!(
                "     {} Redeploy with {}",
                " ".dimmed(),
                "--port <port>".cyan()
            );
        }
        println!();

        // ── Resources ─────────────────────────────────────────────
        if c.cpu_limit.is_some() || c.memory_limit.is_some() {
            println!("   {}", "Resources:".cyan().bold());
            if let Some(cpu) = c.cpu_limit {
                println!("     {} {:.2} vCPU", "CPU:".cyan(), cpu);
            }
            if let Some(mem) = c.memory_limit {
                println!("     {} {} MB", "Memory:".cyan(), mem / 1024 / 1024);
            }
            println!();
        }

        // ── Timing ────────────────────────────────────────────────
        println!("   {}", "Timing:".cyan().bold());
        println!("     {} {}", "Created:".cyan(), c.created_at.dimmed());
        if let Some(s) = &c.started_at {
            println!("     {} {}", "Started:".cyan(), s.dimmed());
        }
        if let Some(f) = &c.finished_at {
            println!("     {} {}", "Stopped:".cyan(), f.dimmed());
            if let Some(code) = c.exit_code {
                let code_str = if code == 0 {
                    format!("{}", code).green()
                } else {
                    format!("{}", code).red()
                };
                println!("     {} {}", "Exit code:".cyan(), code_str);
            }
        }
        if let Some(r) = c.restart_count {
            if r > 0 {
                println!("     {} {}", "Restarts:".cyan(), r.to_string().yellow());
            }
        }

        // ── Command ───────────────────────────────────────────────
        if !c.command.is_empty() {
            println!();
            println!("   {} {}", "Command:".cyan(), c.command.join(" ").dimmed());
        }

        // ── Volumes ───────────────────────────────────────────────
        let real_mounts: Vec<&String> = c
            .volume_mounts
            .iter()
            .filter(|m| !m.starts_with("tmpfs:"))
            .collect();
        if !real_mounts.is_empty() {
            println!();
            println!("   {}", "Volumes:".cyan().bold());
            for mount in &real_mounts {
                println!("     {}", mount.dimmed());
            }
        }

        // ── Environment ───────────────────────────────────────────
        let public_env: Vec<&str> = c
            .env_vars
            .iter()
            .filter(|e| !e.starts_with("NK_") && !e.starts_with("HOME=") && !e.starts_with("PATH="))
            .map(|s| s.as_str())
            .collect();
        if !public_env.is_empty() {
            println!();
            println!("   {}", "Environment:".cyan().bold());
            for e in public_env {
                println!("     {}", e.dimmed());
            }
        }

        // ── Source ────────────────────────────────────────────────
        if oci_source.is_some() || oci_revision.is_some() {
            println!();
            println!("   {}", "Source:".cyan().bold());
            if let Some(src) = oci_source {
                println!("     {} {}", "Repo:".cyan(), src.dimmed());
            }
            if let Some(rev) = oci_revision {
                let short_rev = if rev.len() > 12 { &rev[..12] } else { rev };
                println!("     {} {}", "Commit:".cyan(), short_rev.dimmed());
            }
        }

        Ok(())
    }
}

/// If container is provided, resolve alias and return. Otherwise show interactive selection.
pub(crate) async fn get_container_or_select(
    container: Option<String>,
    json_output: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    match container {
        Some(name) => Ok(resolve_alias(&name)),
        None => {
            if json_output {
                return Err("Container name required in JSON mode".into());
            }
            select_container_interactive().await
        }
    }
}

pub(crate) async fn select_container_interactive() -> Result<String, Box<dyn std::error::Error>> {
    let data = containers::list(&[]).await?;

    if data.containers.is_empty() {
        return Err("No containers found. Deploy one first: nordkraft deploy nginx:alpine".into());
    }

    // Show aliases inline if they exist
    let aliases = load_aliases();
    let reverse_aliases: std::collections::HashMap<&String, &String> =
        aliases.iter().map(|(k, v)| (v, k)).collect();

    let items: Vec<String> = data
        .containers
        .iter()
        .map(|c| {
            let alias_str = reverse_aliases
                .get(&c.name)
                .map(|a| format!(" [{}]", a))
                .unwrap_or_default();
            format!("{}{} ({}) - {}", c.name, alias_str, c.status, c.image)
        })
        .collect();

    println!("{}", "📦 Select a container:".cyan());
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Container")
        .items(&items)
        .default(0)
        .interact_on(&Term::stderr())?;

    Ok(data.containers[selection].name.clone())
}

//! `.nk` specs on the terminal: diff, upgrade, init, edit, specs,
//! spec-set, spec-unset, spec-delete.

use colored::*;
use dialoguer::{theme::ColorfulTheme, Confirm, Select};
use std::error::Error;

use crate::cli::containers::select_container_interactive;
use crate::core::aliases::{load_aliases, resolve_alias};
use crate::core::deploy;
use crate::core::registry::resolve_registry_image;
use crate::core::spec::*;

pub(crate) fn display_diff(diffs: &[SpecDiff], name: &str) {
    let changes: Vec<&SpecDiff> = diffs.iter().filter(|d| d.kind != DiffKind::Same).collect();

    if changes.is_empty() {
        println!(
            "{}",
            format!("✅ {} — spec matches live container", name)
                .green()
                .bold()
        );
        return;
    }

    println!(
        "{}",
        format!("🔍 Diff: {}.nk ↔ live container", name)
            .cyan()
            .bold()
    );
    println!();

    for d in diffs {
        let label = format!("   {:<14}", d.field);
        match d.kind {
            DiffKind::Same => {
                println!(
                    "{}{}   {}",
                    label.dimmed(),
                    d.spec_value.dimmed(),
                    "(unchanged)".dimmed()
                );
            }
            DiffKind::Changed => {
                println!(
                    "{}{}  →  {}   {}",
                    label.white(),
                    d.live_value.red(),
                    d.spec_value.green(),
                    "⚠ changed".yellow()
                );
            }
            DiffKind::Added => {
                println!(
                    "{}{}   {}",
                    label.white(),
                    format!("+ {}", d.spec_value).green(),
                    "(in spec, not live)".dimmed()
                );
            }
            DiffKind::Removed => {
                println!(
                    "{}{}   {}",
                    label.white(),
                    format!("- {}", d.live_value).red(),
                    "(in live, not spec)".dimmed()
                );
            }
        }
    }

    let changed = changes
        .iter()
        .filter(|d| d.kind == DiffKind::Changed)
        .count();
    let added = changes.iter().filter(|d| d.kind == DiffKind::Added).count();
    let removed = changes
        .iter()
        .filter(|d| d.kind == DiffKind::Removed)
        .count();
    println!();

    let mut parts = Vec::new();
    if changed > 0 {
        parts.push(format!("{} changed", changed));
    }
    if added > 0 {
        parts.push(format!("{} added", added));
    }
    if removed > 0 {
        parts.push(format!("{} removed", removed));
    }
    println!(
        "   {}. Run '{}' to apply.",
        parts.join(", "),
        format!("nordkraft upgrade {}", name).cyan()
    );
}

pub(crate) async fn handle_diff_interactive(
    container: Option<String>,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = match container {
        Some(c) => resolve_alias(&c),
        None => select_container_interactive().await?,
    };

    // Load local spec
    let spec = match load_deployment_spec(&name) {
        Some(s) => s,
        None => {
            eprintln!("{} No .nk spec found for '{}'.", "⚠".yellow(), name);
            eprintln!(
                "   Run '{}' to generate one from the running container.",
                format!("nordkraft init {}", name).cyan()
            );
            return Ok(());
        }
    };

    // Fetch live state via inspect
    let live = match fetch_live_spec(&name).await? {
        Some(live) => live,
        None => {
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"status": "no_live_state", "message": "Container has no live state (never started or failed)"})
                );
            } else {
                println!(
                    "{} Container '{}' has no live state (never started or failed deployment).",
                    "⚠".yellow(),
                    name
                );
                println!(
                    "   Run '{}' to apply the spec.",
                    format!("nordkraft upgrade {}", name).cyan()
                );
            }
            return Ok(());
        }
    };
    warn_unread_limits(&live);
    let live = live.spec;

    if json_output {
        let diffs = compute_diff(&spec, &live);
        println!(
            "{}",
            serde_json::to_string_pretty(&diff_changes_json(&diffs))?
        );
    } else {
        let diffs = compute_diff(&spec, &live);
        display_diff(&diffs, &name);
    }

    Ok(())
}

pub(crate) async fn handle_upgrade_interactive(
    container: Option<String>,
    skip_confirm: bool,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = match container {
        Some(c) => resolve_alias(&c),
        None => select_container_interactive().await?,
    };

    // Load local spec
    let spec = match load_deployment_spec(&name) {
        Some(s) => s,
        None => {
            return Err(format!(
                "No .nk spec found for '{}'. Run 'nordkraft init {}' first.",
                name, name
            )
            .into());
        }
    };

    // Fetch live state — may be None for failed/never-started containers
    let live = fetch_live_spec(&name).await?;
    if let Some(live) = &live {
        warn_unread_limits(live);
    }
    let live = live.map(|l| l.spec);
    let is_failed = live.is_none();

    // If we have live state, compute diff; otherwise skip diff and apply directly
    if let Some(ref live) = live {
        let diffs = compute_diff(&spec, live);
        let changes: Vec<&SpecDiff> = diffs.iter().filter(|d| d.kind != DiffKind::Same).collect();

        if changes.is_empty() {
            if !json_output {
                println!(
                    "{}",
                    format!("✅ {} is already up to date", name).green().bold()
                );
            }
            return Ok(());
        }

        if !json_output {
            display_diff(&diffs, &name);
            println!();
        }
    }

    if !json_output {
        if is_failed {
            println!(
                "   {} Container never started — will redeploy from spec.",
                "⚠".yellow()
            );
            println!(
                "   {}",
                format!("Image: {}", spec.deployment.image).dimmed()
            );
            println!();
        }

        if !skip_confirm {
            let prompt = if is_failed {
                "Redeploy from spec?"
            } else {
                "Apply changes?"
            };
            if !is_failed {
                println!(
                    "   {} This will restart the container. Volumes will be preserved.",
                    "⚠".yellow()
                );
            }
            let confirm = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(prompt)
                .default(false)
                .interact()
                .unwrap_or(false);

            if !confirm {
                println!("{}", "   Cancelled.".dimmed());
                return Ok(());
            }
        }

        println!();
        if is_failed {
            println!("{}", format!("🚀 Redeploying {}...", name).cyan());
        } else {
            println!("{}", format!("⏸️  Stopping {}...", name).cyan());
        }
    }

    // Resolve registry:// prefix in image (same as deploy path)
    let resolved_image = resolve_registry_image(&spec.deployment.image)?;
    if resolved_image != spec.deployment.image && !json_output {
        println!(
            "   {} Using private registry: {}",
            "→".dimmed(),
            resolved_image
        );
    }

    // Send the full desired state from the .nk spec
    let result = deploy::upgrade(&name, &spec).await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    // Bump revision in local .nk spec
    let _ = deploy::record_upgrade(&spec, &result);

    println!(
        "{}",
        format!(
            "✅ Upgraded! Revision {} → {}",
            result.revision_old.unwrap_or(0),
            result.revision.unwrap_or(1)
        )
        .green()
        .bold()
    );

    if let Some(ip) = &result.container_ip {
        println!("   {} {}", "IPv4:".cyan(), ip);
    }
    if let Some(image) = &result.image {
        println!("   {} {}", "Image:".cyan(), image);
    }
    println!();
    println!(
        "   {} Updated {}",
        "💾".dimmed(),
        format!("~/.nordkraft/deployments/{}.nk", name).dimmed()
    );

    Ok(())
}

pub(crate) async fn handle_init_interactive(
    container: Option<String>,
    from_server: bool,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = match container {
        Some(c) => resolve_alias(&c),
        None => select_container_interactive().await?,
    };

    if from_server {
        return init_from_server(&name, json_output).await;
    }

    if !json_output {
        println!("{}", format!("🔍 Inspecting {}...", name).cyan());
    }

    let live = match fetch_live_spec(&name).await? {
        Some(live) => {
            warn_unread_limits(&live);
            live.spec
        }
        None => {
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"error": "Container has no live state (never started or failed)"})
                );
            } else {
                eprintln!(
                    "{} Container '{}' has no live state — cannot generate spec from a failed deployment.",
                    "⚠".yellow(), name
                );
                eprintln!(
                    "   Edit the existing .nk spec directly with '{}'.",
                    format!("nordkraft edit {}", name).cyan()
                );
            }
            return Ok(());
        }
    };

    // Check if spec already exists
    if let Some(existing) = load_deployment_spec(&name) {
        if !json_output {
            let overwrite = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!(
                    "{}.nk already exists (revision {}). Overwrite?",
                    name, existing.deployment.revision
                ))
                .default(false)
                .interact()
                .unwrap_or(false);

            if !overwrite {
                println!("{}", "   Cancelled.".dimmed());
                return Ok(());
            }
        }
    }

    save_deployment_spec(&live)?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "spec": nk_path(&name).to_string_lossy(),
                "revision": 0
            }))?
        );
    } else {
        println!();
        println!(
            "   {} Generated {}",
            "💾".green(),
            format!("~/.nordkraft/deployments/{}.nk", name).cyan()
        );
        println!("   {} revision 0 (from running container)", "→".dimmed());
        println!();
        println!(
            "   Edit and run '{}' to apply changes.",
            format!("nordkraft upgrade {}", name).cyan()
        );
    }

    Ok(())
}

pub(crate) async fn handle_edit_interactive(
    container: Option<String>,
    _json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = match container {
        Some(c) => resolve_alias(&c),
        None => {
            // Try to pick from existing specs
            let specs = list_deployment_specs();
            if specs.is_empty() {
                return Err("No .nk specs found. Run 'nordkraft init <container>' first.".into());
            }
            // Show the alias first so the picker reads "nginx  (app-b006…)"
            // instead of a bare container id.
            let aliases = load_aliases();
            let reverse_aliases: std::collections::HashMap<&String, &String> =
                aliases.iter().map(|(alias, name)| (name, alias)).collect();
            let display_items: Vec<String> = specs
                .iter()
                .map(|name| match reverse_aliases.get(name) {
                    Some(alias) => {
                        let short: String = if name.chars().count() > 30 {
                            format!("{}…", name.chars().take(29).collect::<String>())
                        } else {
                            name.clone()
                        };
                        format!("{}  ({})", alias, short)
                    }
                    None => name.clone(),
                })
                .collect();
            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Select spec to edit")
                .items(&display_items)
                .default(0)
                .interact()?;
            specs[selection].clone()
        }
    };

    let path = nk_path(&name);
    if !path.exists() {
        return Err(format!(
            "No .nk spec found for '{}'. Run 'nordkraft init {}' first.",
            name, name
        )
        .into());
    }

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());

    // Read content before edit for change detection
    let before = std::fs::read_to_string(&path)?;

    let status = std::process::Command::new(&editor).arg(&path).status()?;

    if !status.success() {
        return Err(format!("{} exited with error", editor).into());
    }

    let after = std::fs::read_to_string(&path)?;

    if before == after {
        println!("{}", "   No changes made.".dimmed());
        return Ok(());
    }

    // Validate TOML
    match toml::from_str::<DeploymentSpec>(&after) {
        Ok(_) => {
            println!("{}", "📝 Spec updated.".green());
            println!();

            let show_diff = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Show diff against live container?")
                .default(true)
                .interact()
                .unwrap_or(false);

            if show_diff {
                let _ = handle_diff_interactive(Some(name), false).await;
            }
        }
        Err(e) => {
            eprintln!("{} Invalid TOML after edit: {}", "⚠".yellow(), e);
            eprintln!("   The file was saved but may not be valid.");
        }
    }

    Ok(())
}

pub(crate) async fn handle_specs_list(json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    let specs = list_deployment_specs();

    if specs.is_empty() {
        if !json_output {
            println!("{}", "No deployment specs found.".dimmed());
            println!(
                "   Deploy a container or run '{}' to generate one.",
                "nordkraft init <container>".cyan()
            );
        }
        return Ok(());
    }

    // Build reverse alias map: container_name → alias
    let aliases = load_aliases();
    let reverse_aliases: std::collections::HashMap<&String, &String> =
        aliases.iter().map(|(alias, name)| (name, alias)).collect();

    if json_output {
        println!("{}", serde_json::to_string_pretty(&crate::mcp::spec_summaries())?);
        return Ok(());
    }

    // Shorten a raw container id like `app-51d83cde-cf29-4739-a057-35a088e2f4ad`
    // to `app-51d83cde…f4ad` so it fits a narrow name column.
    fn short_id(id: &str) -> String {
        if id.len() <= 18 {
            return id.to_string();
        }
        let head: String = id.chars().take(12).collect();
        let tail: String = id
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("{}…{}", head, tail)
    }

    // Truncate long image names from the left, keeping the tag visible.
    fn short_image(img: &str) -> String {
        const MAX: usize = 44;
        if img.len() <= MAX {
            return img.to_string();
        }
        format!("…{}", &img[img.len() - (MAX - 1)..])
    }

    // Pick a status dot based on the spec's revision.
    // r0   → green  (fresh / never upgraded)
    // r1+  → cyan   (has been upgraded at least once)
    fn spec_icon(revision: u64) -> colored::ColoredString {
        if revision == 0 {
            "●".bright_green().bold()
        } else {
            "●".bright_cyan().bold()
        }
    }

    // Load all rows up front so we can compute column widths.
    let rows: Vec<(String, DeploymentSpec)> = specs
        .iter()
        .filter_map(|name| load_deployment_spec(name).map(|s| (name.clone(), s)))
        .collect();

    // Primary display name for each row: alias if present, else shortened id.
    let display_names: Vec<String> = rows
        .iter()
        .map(|(_, s)| {
            reverse_aliases
                .get(&s.deployment.name)
                .map(|a| a.to_string())
                .unwrap_or_else(|| short_id(&s.deployment.name))
        })
        .collect();

    // Tight, capped column width — no single row gets to stretch the whole table.
    const NAME_CAP: usize = 20;
    let name_width = display_names
        .iter()
        .map(|n| n.len())
        .max()
        .unwrap_or(12)
        .clamp(12, NAME_CAP);

    println!();
    println!(
        "   📋 {}",
        format!("Saved deployment specs (.nk files) — {} total", rows.len())
            .cyan()
            .bold()
    );
    println!(
        "   {}",
        "These are on-disk manifests. Use 'nordkraft list' to see running containers.".dimmed()
    );
    println!(
        "   {}  {}   {}  {}",
        "●".bright_green().bold(),
        "new".dimmed(),
        "●".bright_cyan().bold(),
        "upgraded".dimmed()
    );
    println!();

    for ((_, spec), display) in rows.iter().zip(display_names.iter()) {
        let has_alias = reverse_aliases.contains_key(&spec.deployment.name);
        let secondary: Option<String> = if has_alias || spec.deployment.name.len() > display.len() {
            Some(spec.deployment.name.clone())
        } else {
            None
        };

        let icon = spec_icon(spec.deployment.revision.into());

        // Line 1: icon · name (aligned) · revision · image
        println!(
            "   {}  {:<width$}  {}  {}",
            icon,
            display.white().bold(),
            format!("r{}", spec.deployment.revision).yellow(),
            short_image(&spec.deployment.image).cyan(),
            width = name_width
        );

        // Line 2: dimmed metadata — cpu, mem, ports, ipv6; full id appended if available.
        // Four leading spaces to align under the name (past the icon + its padding).
        let ports_str = if spec.network.ports.is_empty() {
            "no ports".to_string()
        } else {
            spec.network
                .ports
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut meta = format!(
            "{} cpu · {} mem · {} · ipv6={}",
            spec.resources.cpu, spec.resources.memory, ports_str, spec.network.ipv6
        );
        if let Some(id) = secondary {
            meta.push_str(&format!("  ·  {}", id));
        }
        println!(
            "       {:<width$}  {}",
            "",
            meta.dimmed(),
            width = name_width
        );
    }

    // Flag invalid .nk files that didn't load.
    for name in &specs {
        if !rows.iter().any(|(n, _)| n == name) {
            println!(
                "   {}  {} {}",
                "●".bright_red().bold(),
                name.white().bold(),
                "(invalid .nk file)".red()
            );
        }
    }

    println!();
    println!(
        "   {} {}   {}",
        "→".dimmed(),
        "nordkraft diff <name>".cyan(),
        "compare spec vs live".dimmed()
    );
    println!(
        "   {} {}   {}",
        "→".dimmed(),
        "nordkraft edit <name>".cyan(),
        "open in $EDITOR".dimmed()
    );
    println!(
        "   {} {}   {}",
        "→".dimmed(),
        "nordkraft spec-set <name> <key> <val>".cyan(),
        "edit a field".dimmed()
    );

    Ok(())
}

/// `nordkraft init <name> --from-server`
///
/// Rebuilds the .nk from what was requested at deploy time instead of from the
/// running container, so nothing is approximated and the spec keeps the server's
/// revision rather than resetting to 0.
pub(crate) async fn init_from_server(
    name: &str,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !json_output {
        println!(
            "{}",
            format!("🗄  Fetching stored config for {}...", name).cyan()
        );
    }

    let stored = match fetch_stored_spec(name).await? {
        Some(spec) => spec,
        None => {
            let msg = format!(
                "No stored config for '{}' — deployed before config tracking. \
                 Rebuild from the running container with 'nordkraft init {}'.",
                name, name
            );
            if json_output {
                println!("{}", serde_json::json!({ "error": msg }));
            } else {
                eprintln!("{} {}", "⚠".yellow(), msg);
            }
            return Ok(());
        }
    };

    if let Some(existing) = load_deployment_spec(name) {
        if !json_output {
            let overwrite = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!(
                    "{}.nk already exists (revision {}). Overwrite from server?",
                    name, existing.deployment.revision
                ))
                .default(false)
                .interact()
                .unwrap_or(false);
            if !overwrite {
                println!("{}", "   Cancelled.".dimmed());
                return Ok(());
            }
        }
    }

    let revision = stored.deployment.revision;
    save_deployment_spec(&stored)?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "spec": nk_path(name).to_string_lossy(),
                "revision": revision,
                "source": "server"
            }))?
        );
    } else {
        println!();
        println!(
            "   {} Generated {}",
            "💾".green(),
            format!("~/.nordkraft/deployments/{}.nk", name).cyan()
        );
        println!(
            "   {} revision {} (from stored deploy config)",
            "→".dimmed(),
            revision
        );
    }

    Ok(())
}

/// `nordkraft spec set <app> <key> <value> [--apply]`
///
/// Supports three value modes on the RHS:
///   plain          → replace
///   +item          → append to array (e.g. `network.ports +8080:80`)
///   -item          → remove from array
pub async fn handle_spec_set(
    container: String,
    key: String,
    raw_value: String,
    apply: bool,
    _json_output: bool,
) -> Result<(), Box<dyn Error>> {
    let name = resolve_alias(&container);
    let (path, outcome) = set_field(&name, &key, &raw_value)?;
    match outcome {
        SpecSetOutcome::Appended(item) => {
            println!("{} {} += {}", "✔".green(), key.bold(), item);
        }
        SpecSetOutcome::Removed(item) => {
            println!("{} {} -= {}", "✔".green(), key.bold(), item);
        }
        SpecSetOutcome::Replaced { created } => {
            if created {
                eprintln!(
                    "{} '{}' did not exist before — creating new field.",
                    "ℹ".cyan(),
                    key
                );
            }
            println!("{} {} = {}", "✔".green(), key.bold(), raw_value);
        }
    }
    println!("   {} {}", "Saved:".dimmed(), path.display());

    if apply {
        println!();
        println!("{}", "→ Applying to running container…".cyan());
        handle_upgrade_interactive(Some(name), true, false).await?;
    } else {
        println!(
            "   {} nordkraft upgrade {}",
            "Apply with:".dimmed(),
            container
        );
    }
    Ok(())
}

/// `nordkraft spec unset <app> <key>`
pub async fn handle_spec_unset(
    container: String,
    key: String,
    _json_output: bool,
) -> Result<(), Box<dyn Error>> {
    let name = resolve_alias(&container);
    let path = unset_field(&name, &key)?;
    println!("{} removed {}", "✔".green(), key.bold());
    println!("   {} {}", "Saved:".dimmed(), path.display());
    Ok(())
}

/// `nordkraft spec delete <app> [--yes]`
///
/// Deletes the .nk file only. Does NOT touch the running container — that's
/// `nordkraft destroy`. We print a reminder if a spec is removed while a
/// container by the same name might still exist.
pub async fn handle_spec_delete(
    container: String,
    yes: bool,
    _json_output: bool,
) -> Result<(), Box<dyn Error>> {
    let name = resolve_alias(&container);
    let path = nk_path(&name);
    if !path.exists() {
        return Err(format!("No .nk spec found for '{}'", name).into());
    }

    if !yes {
        let confirmed = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Delete spec file for '{}'? (container will NOT be destroyed)",
                name
            ))
            .default(false)
            .interact()
            .unwrap_or(false);
        if !confirmed {
            println!("{}", "   Cancelled.".dimmed());
            return Ok(());
        }
    }

    delete_spec(&name)?;
    println!("{} deleted {}", "✔".green(), path.display());
    println!(
        "   {} If '{}' is still running, destroy it with: nordkraft destroy {}",
        "Note:".yellow(),
        name,
        name
    );
    Ok(())
}

/// Say which limits the live spec had to default, before anything is
/// compared or written on the strength of them.
pub(crate) fn warn_unread_limits(live: &LiveSpec) {
    if live.unread_limits.is_empty() {
        return;
    }
    eprintln!(
        "{} Could not read {} from the running container — wrote defaults (0.5 cpu / 512m).",
        "⚠".yellow(),
        live.unread_limits.join(" and ")
    );
    eprintln!("   Verify against the node before running 'nordkraft upgrade' on this container.");
}

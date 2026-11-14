//! `nordkraft deploy` — prompts, progress and the result on the terminal.

use colored::*;
use console::Term;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use std::collections::HashMap;

use crate::cli::containers::{handle_container_inspect, handle_container_remove_opts};
use crate::core::aliases::set_alias;
use crate::core::api::*;
use crate::core::deploy::{self, DeployError, DeployInput};
use crate::core::registry::resolve_registry_image;
use crate::core::spec::*;
use crate::{non_interactive, volume, DeployArgs};

pub(crate) async fn handle_deploy(
    args: DeployArgs,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // If --from is set, load .nk spec and build args from it.
    // CLI flags (--name, --alias) are preserved alongside the spec.
    let mut args = if let Some(from) = &args.from {
        let spec = resolve_and_load_spec(from)?;

        if !json_output {
            println!(
                "{}",
                format!("📄 Loading spec: {}", spec.deployment.name).cyan()
            );
        }

        // Build DeployArgs from spec, preserving --name and --alias from CLI
        DeployArgs {
            image: spec.deployment.image.clone(),
            from: args.from.clone(),
            port: spec.network.ports.clone(),
            env: spec
                .env
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect(),
            env_file: None,
            cpu: spec.resources.cpu,
            memory: spec.resources.memory.clone(),
            persistence: spec.storage.persistence,
            volume_path: spec.storage.volume_path.clone(),
            volume_size: spec.storage.volume_size.clone(),
            volume_from: args.volume_from.clone(),
            ipv6: spec.network.ipv6,
            garage: spec.placement.garage.clone(),
            hardware: spec.placement.hardware.clone(),
            // --name: use CLI value if provided, otherwise spec name
            name: args.name.or(Some(spec.deployment.name.clone())),
            alias: args.alias.clone(),
            command: spec.deployment.command.clone(),
            restart: spec.lifecycle.restart.clone(),
            auto_stop: spec.lifecycle.auto_stop.clone(),
            auto_delete: spec.lifecycle.auto_delete.clone(),
            max_lifetime: spec.lifecycle.max_lifetime.clone(),
            label: spec
                .labels
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect(),
        }
    } else if args.image.is_empty() && !json_output {
        // If no image provided and not JSON mode, offer interactive
        prompt_deploy_interactive()?
    } else if args.image.is_empty() {
        return Err(
            "Image required. Use: nordkraft deploy <image> or nordkraft deploy --from <spec.nk>"
                .into(),
        );
    } else {
        args
    };

    // Explicit private registry references: "registry://myapp:v1"
    // Resolves to the user's private registry address.
    if args.image.starts_with("registry://") {
        args.image = resolve_registry_image(&args.image)?;
        if !json_output {
            println!("   {} Using private registry: {}", "→".dimmed(), args.image);
        }
    }

    // Environment: file first, command line overrides
    let mut env_vars = match &args.env_file {
        Some(env_file) => deploy::parse_env_file(env_file)?,
        None => HashMap::new(),
    };
    deploy::parse_env_pairs(&args.env, &mut env_vars)?;

    // Ports — prompt interactively if no --port flag given
    let ports: Vec<u16> = if !args.port.is_empty() {
        args.port.clone()
    } else if json_output {
        // Non-interactive mode: no default port, let server decide
        vec![]
    } else {
        prompt_ports(&args.image)
    };

    // Validate: --volume-from needs a persistent volume to land in, and a real file
    if let Some(file) = &args.volume_from {
        if !args.persistence {
            return Err("--volume-from requires --persistence and --volume-path".into());
        }
        if !std::path::Path::new(file).is_file() {
            return Err(format!("--volume-from: {} is not a file", file).into());
        }
    }

    let input = DeployInput {
        image: args.image.clone(),
        ports,
        env: env_vars,
        cpu: args.cpu,
        memory: args.memory.clone(),
        persistence: args.persistence,
        volume_path: args.volume_path.clone(),
        volume_size: args.volume_size.clone(),
        ipv6: args.ipv6,
        garage: args.garage.clone(),
        hardware: args.hardware.clone(),
        command: args.command.clone(),
        lifecycle: LifecycleSpec::from_flags(
            &args.restart,
            &args.auto_stop,
            &args.auto_delete,
            &args.max_lifetime,
        ),
        labels: labels_from_flags(&args.label),
        // A restore right after deploy must not race the node: wait until the
        // container is really there before pushing the volume into it.
        wait: args.volume_from.is_some(),
    };

    // Validate: --persistence requires --volume-path
    if let Err(e) = deploy::validate(&input) {
        if json_output {
            return Err(e);
        }
        eprintln!(
            "{}",
            "❌ Error: --persistence requires --volume-path"
                .red()
                .bold()
        );
        eprintln!();
        eprintln!("   You must specify where inside the container to mount persistent storage.");
        eprintln!();
        eprintln!("   {}", "Common paths by image:".yellow());
        eprintln!("     Rails apps:     --volume-path /rails/storage");
        eprintln!("     PostgreSQL:     --volume-path /var/lib/postgresql/data");
        eprintln!("     MySQL:          --volume-path /var/lib/mysql");
        eprintln!("     Redis:          --volume-path /data");
        eprintln!("     Generic apps:   --volume-path /data");
        eprintln!();
        eprintln!("   {}", "Example:".yellow());
        eprintln!("     nordkraft deploy ghcr.io/basecamp/once-campfire:latest \\");
        eprintln!("       --persistence --volume-path /rails/storage");
        eprintln!();
        return Err("Missing required --volume-path".into());
    }

    let request = deploy::build_request(&input)?;

    if !json_output {
        println!("{}", format!("🚀 Deploying {}...", args.image).cyan());
        if args.ipv6 {
            println!("   {} IPv6 enabled", "→".dimmed());
        }
        if args.persistence {
            println!(
                "   {} Persistent storage at {}",
                "→".dimmed(),
                args.volume_path.as_ref().unwrap().cyan()
            );
        }
        if request.wait {
            println!(
                "   {} waiting for the container to start before restoring",
                "→".dimmed()
            );
        }
    }

    let result = match deploy::deploy(&request).await {
        Ok(result) => result,
        Err(DeployError::Quota(error)) => {
            print_quota_error(&error);
            return Err("Quota exceeded".into());
        }
        Err(e) => return Err(e.to_string().into()),
    };

    // --volume-from: push the backup into the new volume. A failed restore
    // removes the container again so nothing half-made is left behind.
    let mut restore_report: Option<serde_json::Value> = None;
    if let (Some(file), Some(name)) = (&args.volume_from, &result.container_name) {
        match volume::restore_file(name, std::path::Path::new(file), json_output).await {
            Ok(report) => {
                if !json_output {
                    volume::print_restore_summary(&report);
                }
                restore_report = Some(report);
            }
            Err(e) => {
                if !json_output {
                    eprintln!("{} restore failed: {}", "❌".red(), e);
                    eprintln!("   removing {} again", name);
                }
                let _ = handle_container_remove_opts(name, true, true).await;
                return Err(format!("deploy rolled back: restore failed: {e}").into());
            }
        }
    }

    if json_output {
        let mut out = serde_json::to_value(&result)?;
        if let Some(r) = restore_report {
            out["restore"] = r;
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("{}", "✅ Container deployed!".green().bold());
    println!();
    if let Some(name) = &result.container_name {
        println!("   {} {}", "Name:".cyan(), name.white().bold());

        // If --alias was explicitly provided, set it directly (no prompt)
        if let Some(alias_value) = &args.alias {
            if !alias_value.is_empty() {
                if let Err(e) = set_alias(alias_value, name) {
                    eprintln!("   {} Failed to set alias: {}", "⚠".yellow(), e);
                } else {
                    println!(
                        "   {} Alias '{}' → '{}'",
                        "✅".green(),
                        alias_value.cyan(),
                        name
                    );
                }
            }
        }
        // If --name was provided, user already chose a name — skip alias prompt
        else if args.name.is_some() {
            // No alias prompt — user explicitly named this container
        }
        // Otherwise, offer interactive alias prompt (unless non-interactive)
        else if non_interactive() {
            // --yes / --json / no TTY: no alias, no prompt
        } else {
            let set_alias_prompt = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Set a short alias for this container?")
                .default(true)
                .interact();

            if let Ok(true) = set_alias_prompt {
                let alias: String = Input::with_theme(&ColorfulTheme::default())
                    .with_prompt("Alias")
                    .interact_text()
                    .unwrap_or_default();

                if !alias.is_empty() {
                    if let Err(e) = set_alias(&alias, name) {
                        eprintln!("   {} Failed to set alias: {}", "⚠".yellow(), e);
                    } else {
                        println!("   {} Alias '{}' → '{}'", "✅".green(), alias.cyan(), name);
                    }
                }
            }
        }
    }
    if let Some(garage) = &result.garage {
        println!("   {} {}", "Garage:".cyan(), garage);
    }
    if let Some(node) = &result.node {
        println!("   {} {}", "Node:".cyan(), node);
    }
    if !result.access_urls.is_empty() {
        for url in &result.access_urls {
            println!("   {} {}", "URL:".cyan(), url.cyan().bold());
        }
        if result.network_mode.as_deref() == Some("bridge") {
            println!(
                "   {}",
                "(bridge mode: reached via the host's published port, not the container IP)"
                    .dimmed()
            );
        }
    } else if let Some(ip) = &result.container_ip {
        println!("   {} {}", "IPv4:".cyan(), ip);
    }
    if let Some(ipv6) = &result.ipv6_address {
        println!("   {} {}", "IPv6:".cyan(), ipv6.cyan().bold());
        println!();
        println!(
            "   {} Open firewall: nordkraft ipv6 open {}",
            "💡".yellow(),
            result.container_name.as_deref().unwrap_or("<name>")
        );
    }
    println!();
    println!(
        "   {} nordkraft logs {}",
        "View logs:".dimmed(),
        result.container_name.as_deref().unwrap_or("<name>")
    );

    // Save .nk deployment spec
    if let Some(name) = &result.container_name {
        let spec = deploy::spec_for(&input, name, result.garage.as_deref());
        match save_deployment_spec(&spec) {
            Ok(_) if !json_output => {
                println!();
                println!(
                    "   {} Saved deployment spec → {}",
                    "💾".dimmed(),
                    format!("~/.nordkraft/deployments/{}.nk", name).dimmed()
                );
            }
            Err(e) if !json_output => {
                eprintln!("   {} Could not save .nk spec: {}", "⚠".yellow(), e);
            }
            _ => {}
        }
    }

    // Prompt to inspect — verify runtime, ports, volumes are correct
    if let (Some(name), false) = (&result.container_name, non_interactive()) {
        println!();
        let inspect_prompt = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Inspect container now?")
            .default(false)
            .interact();

        if let Ok(true) = inspect_prompt {
            tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;
            let _ = handle_container_inspect(name, json_output).await;
        }
    }

    Ok(())
}

/// Ask which port to expose, defaulting from the image name.
fn prompt_ports(image: &str) -> Vec<u16> {
    println!();
    match deploy::suggest_port(image) {
        None => {
            // DB or no-port image — ask explicitly
            let expose = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Does this container expose a port?")
                .default(false)
                .interact()
                .unwrap_or(false);

            if expose {
                let port_input: String = Input::with_theme(&ColorfulTheme::default())
                    .with_prompt("Port")
                    .interact_text()
                    .unwrap_or_default();
                port_input
                    .trim()
                    .parse::<u16>()
                    .map(|p| vec![p])
                    .unwrap_or_default()
            } else {
                vec![]
            }
        }
        Some(default_port) => {
            let port_str: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Port to expose")
                .default(default_port.to_string())
                .show_default(true)
                .interact_text()
                .unwrap_or(default_port.to_string());

            if port_str.trim() == "none" || port_str.trim().is_empty() {
                vec![]
            } else {
                port_str
                    .trim()
                    .parse::<u16>()
                    .map(|p| vec![p])
                    .unwrap_or_else(|_| vec![default_port])
            }
        }
    }
}

pub(crate) fn prompt_deploy_interactive() -> Result<DeployArgs, Box<dyn std::error::Error>> {
    println!("{}", "🚀 Interactive Deploy".cyan().bold());
    println!();

    // Image selection
    let common_images = vec![
        "nginx:alpine",
        "httpd:alpine",
        "node:20-alpine",
        "python:3.12-alpine",
        "postgres:16-alpine",
        "redis:alpine",
        "Custom image...",
    ];

    let image_idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select image")
        .items(&common_images)
        .default(0)
        .interact_on(&Term::stderr())?;

    let image = if image_idx == common_images.len() - 1 {
        Input::<String>::with_theme(&ColorfulTheme::default())
            .with_prompt("Enter custom image")
            .interact_text()?
    } else {
        common_images[image_idx].to_string()
    };

    // Port
    let port_str: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Port to expose")
        .default("80".to_string())
        .interact_text()?;
    let port: u16 = port_str.parse().unwrap_or(80);

    // IPv6
    let enable_ipv6 = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Enable IPv6 direct access?")
        .default(false)
        .interact()?;

    // Persistence
    let enable_persistence = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Enable persistent storage?")
        .default(false)
        .interact()?;

    // Volume path (required if persistence enabled)
    let volume_path = if enable_persistence {
        let common_paths = vec![
            "/data",
            "/rails/storage",
            "/var/lib/postgresql/data",
            "/var/lib/mysql",
            "/app/data",
            "Custom path...",
        ];

        println!();
        println!("   {} Where does your app store data?", "📁".yellow());

        let path_idx = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Volume mount path")
            .items(&common_paths)
            .default(0)
            .interact_on(&Term::stderr())?;

        let path = if path_idx == common_paths.len() - 1 {
            Input::<String>::with_theme(&ColorfulTheme::default())
                .with_prompt("Enter container path")
                .interact_text()?
        } else {
            common_paths[path_idx].to_string()
        };
        Some(path)
    } else {
        None
    };

    // Memory
    let memory_options = vec!["256m", "512m", "1g", "2g"];
    let mem_idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Memory limit")
        .items(&memory_options)
        .default(1)
        .interact_on(&Term::stderr())?;

    let volume_size = if enable_persistence {
        let size_options = vec!["512m", "1g", "2g", "5g", "10g"];
        let size_idx = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Volume size")
            .items(&size_options)
            .default(1) // 1g
            .interact_on(&Term::stderr())?;
        size_options[size_idx].to_string()
    } else {
        "1g".to_string()
    };

    Ok(DeployArgs {
        image,
        from: None,
        port: vec![port],
        env: vec![],
        env_file: None,
        cpu: 0.5,
        memory: memory_options[mem_idx].to_string(),
        persistence: enable_persistence,
        volume_path,
        ipv6: enable_ipv6,
        garage: None,
        hardware: None,
        name: None,
        command: None,
        alias: None,
        volume_size,
        restart: None,
        auto_stop: None,
        auto_delete: None,
        max_lifetime: None,
        label: vec![],
        volume_from: None,
    })
}

pub(crate) fn print_quota_error(error: &ApiError) {
    eprintln!("{}", "❌ Deploy blocked — plan quota exceeded".red().bold());
    eprintln!();
    if let Some(plan) = &error.plan {
        eprintln!("   {} {}", "Plan:".cyan(), plan.white().bold());
    }
    if let Some(usage) = &error.usage {
        for key in &["cpu", "memory", "disk"] {
            if let Some(val) = usage.get(key).and_then(|v| v.as_str()) {
                eprintln!("   {} {}", format!("{}:", key.to_uppercase()).cyan(), val);
            }
        }
        if let Some(count) = usage.get("containers").and_then(|v| v.as_i64()) {
            eprintln!("   {} {}", "Containers:".cyan(), count);
        }
    }
    eprintln!();
    eprintln!("   {}", error.error.yellow());
}

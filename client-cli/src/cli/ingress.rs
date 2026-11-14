//! `nordkraft ingress` on the terminal.

use colored::*;

use crate::core::aliases::resolve_alias;
use crate::core::ingress;
use crate::IngressCommands;

pub(crate) async fn handle_ingress(
    command: IngressCommands,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        IngressCommands::Enable {
            container,
            subdomain,
            port,
            mode,
        } => {
            let container = resolve_alias(&container);
            if !json_output {
                println!(
                    "{}",
                    format!("🌐 Enabling ingress for {}...", container).cyan()
                );
            }

            let result = ingress::enable(&container, &subdomain, port, &mode).await?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", "✅ Ingress enabled!".green().bold());
                if let Some(url) = result.url {
                    println!();
                    println!("   {} {}", "URL:".cyan(), url.white().bold());
                }
                if result.ssl.unwrap_or(false) {
                    println!("   {} ✓ Auto-TLS enabled", "SSL:".cyan());
                }
            }
        }
        IngressCommands::Disable { container } => {
            let container = resolve_alias(&container);
            if !json_output {
                println!(
                    "{}",
                    format!("🔒 Disabling ingress for {}...", container).cyan()
                );
            }

            let result = ingress::disable(&container).await?;

            if json_output {
                // The controller's body when it was JSON, else a simple success message
                match result {
                    Some(result) => println!("{}", serde_json::to_string_pretty(&result)?),
                    None => println!("{}", serde_json::json!({"status": "disabled"})),
                }
            } else {
                println!("{} Ingress disabled", "✅".green().bold());
            }
        }
        IngressCommands::Status { container } => {
            let container = resolve_alias(&container);
            let result = ingress::status(&container).await?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", format!("🌐 Ingress status for {}:", container).cyan());
                println!();
                println!(
                    "   {} {}",
                    "Enabled:".cyan(),
                    if result.enabled {
                        "Yes".green()
                    } else {
                        "No".yellow()
                    }
                );
                if let Some(url) = result.url {
                    println!("   {} {}", "URL:".cyan(), url);
                }
                if let Some(port) = result.target_port {
                    println!("   {} {}", "Target port:".cyan(), port);
                }
                if let Some(ssl) = result.ssl {
                    println!(
                        "   {} {}",
                        "SSL:".cyan(),
                        if ssl { "✓ Enabled" } else { "✗ Disabled" }
                    );
                }
            }
        }
        IngressCommands::List => {
            let result = ingress::list().await?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if result.routes.is_empty() {
                println!("{}", "No ingress routes configured.".yellow());
                println!("   Enable one: nordkraft ingress enable <container> --subdomain myapp");
            } else {
                println!(
                    "{}",
                    format!("🌐 {} ingress route(s):", result.routes.len())
                        .green()
                        .bold()
                );
                println!();
                for route in &result.routes {
                    let status = if route.is_active {
                        "active".green()
                    } else {
                        "inactive".yellow()
                    };
                    let mode = route.mode.as_deref().unwrap_or("https");
                    println!(
                        "   {} → {}",
                        route.subdomain.cyan().bold(),
                        route.url.white()
                    );
                    println!(
                        "     Container: {} | Port: {} | Mode: {} | Status: {}",
                        route.container_id.dimmed(),
                        route.target_port,
                        mode,
                        status
                    );
                    if let Some(ip) = &route.target_ip {
                        println!("     Target IP: {}", ip.dimmed());
                    }
                    println!();
                }
            }
        }
    }

    Ok(())
}

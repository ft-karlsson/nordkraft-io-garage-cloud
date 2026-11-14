//! `nordkraft usage` and `nordkraft events` on the terminal.

use colored::*;

use crate::core::aliases::resolve_alias;
use crate::core::usage;

pub(crate) async fn handle_usage(json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    let data = usage::usage().await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }

    let plan_name = data["plan"]["name"].as_str().unwrap_or("Unknown");
    let cpu_used = data["usage"]["cpu"].as_f64().unwrap_or(0.0);
    let cpu_max = data["plan"]["limits"]["cpu"].as_f64().unwrap_or(1.0);
    let mem_used = data["usage"]["memory_mb"].as_i64().unwrap_or(0);
    let mem_max = data["plan"]["limits"]["memory_mb"].as_i64().unwrap_or(512);
    let containers = data["usage"]["containers"].as_i64().unwrap_or(0);
    let cpu_ratio = data["ratios"]["cpu"].as_f64().unwrap_or(0.0);
    let mem_ratio = data["ratios"]["memory"].as_f64().unwrap_or(0.0);

    println!(
        "{}",
        format!("📊 Plan: {}  (allocated resources)", plan_name)
            .cyan()
            .bold()
    );
    println!();

    // CPU bar
    let cpu_bar = render_bar(cpu_ratio, 30);
    let cpu_color = if cpu_ratio < 0.6 {
        "green"
    } else if cpu_ratio < 0.85 {
        "yellow"
    } else {
        "red"
    };
    print!("   CPU      ");
    print_colored_bar(&cpu_bar, cpu_color);
    println!("  {:.1}/{:.1} vCPU", cpu_used, cpu_max);

    // Memory bar
    let mem_bar = render_bar(mem_ratio, 30);
    let mem_color = if mem_ratio < 0.6 {
        "green"
    } else if mem_ratio < 0.85 {
        "yellow"
    } else {
        "red"
    };
    print!("   Memory   ");
    print_colored_bar(&mem_bar, mem_color);
    println!("  {}MB/{}MB", mem_used, mem_max);

    // Disk bar
    let disk_used = data["usage"]["disk_mb"].as_i64().unwrap_or(0);
    let disk_max = data["plan"]["limits"]["storage_mb"]
        .as_i64()
        .unwrap_or(102400);
    let disk_ratio = data["ratios"]["disk"].as_f64().unwrap_or(0.0);
    let disk_bar = render_bar(disk_ratio, 30);
    let disk_color = if disk_ratio < 0.6 {
        "green"
    } else if disk_ratio < 0.85 {
        "yellow"
    } else {
        "red"
    };
    print!("   Disk     ");
    print_colored_bar(&disk_bar, disk_color);
    println!(
        "  {:.1}/{:.0}GB",
        disk_used as f64 / 1024.0,
        disk_max as f64 / 1024.0
    );

    println!();
    println!("   {} {} active", "Containers:".dimmed(), containers);

    Ok(())
}

pub(crate) async fn handle_events(
    container: Option<String>,
    limit: usize,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Resolve alias if provided
    let container = container.map(|c| resolve_alias(&c));

    let data = usage::events(container.as_deref(), limit).await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&data)?);
        return Ok(());
    }

    let events = data["events"].as_array();

    if events.is_none() || events.unwrap().is_empty() {
        println!("{}", "No deploy events found.".dimmed());
        if container.is_some() {
            println!("   Try without filter: {}", "nordkraft events".cyan());
        }
        return Ok(());
    }

    let events = events.unwrap();

    if let Some(ref name) = container {
        println!(
            "{}",
            format!("📋 Deploy events for: {}", name).cyan().bold()
        );
    } else {
        println!("{}", "📋 Deploy events".cyan().bold());
    }
    println!();

    // Events come newest-first from API, reverse for chronological display
    for event in events.iter().rev() {
        let phase = event["phase"].as_str().unwrap_or("?");
        let message = event["message"].as_str().unwrap_or("");
        let success = event["success"].as_bool().unwrap_or(true);
        let timestamp = event["created_at"].as_str().unwrap_or("");
        let container_name = event["container_name"].as_str().unwrap_or("");

        // Format timestamp to just time if today
        let time_str = if timestamp.len() > 19 {
            &timestamp[11..19]
        } else {
            timestamp
        };

        let phase_icon = match phase {
            "network" => "🌐",
            "pulling" => "⬇️ ",
            "pulled" => "📦",
            "created" => "🔧",
            "starting" => "🚀",
            "running" => "✅",
            "upgrading" => "🔄",
            "failed" => "❌",
            _ => "  ",
        };

        let phase_colored = if success {
            match phase {
                "running" => phase.green().bold().to_string(),
                "pulling" | "upgrading" => phase.cyan().to_string(),
                _ => phase.white().to_string(),
            }
        } else {
            phase.red().bold().to_string()
        };

        // Show container name only in unfiltered view
        if container.is_none() {
            println!(
                "   {} {} {} {} {}",
                time_str.dimmed(),
                phase_icon,
                phase_colored,
                container_name.white().bold(),
                message.dimmed()
            );
        } else {
            println!(
                "   {} {} {} {}",
                time_str.dimmed(),
                phase_icon,
                phase_colored,
                message.dimmed()
            );
        }
    }

    println!();

    Ok(())
}

pub(crate) fn render_bar(ratio: f64, width: usize) -> (usize, usize) {
    let ratio = ratio.clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let empty = width - filled;
    (filled, empty)
}

pub(crate) fn print_colored_bar((filled, empty): &(usize, usize), color: &str) {
    let filled_str = "█".repeat(*filled);
    let empty_str = "░".repeat(*empty);
    match color {
        "green" => print!("{}{}", filled_str.green(), empty_str.dimmed()),
        "yellow" => print!("{}{}", filled_str.yellow(), empty_str.dimmed()),
        "red" => print!("{}{}", filled_str.red(), empty_str.dimmed()),
        _ => print!("{}{}", filled_str, empty_str.dimmed()),
    }
}

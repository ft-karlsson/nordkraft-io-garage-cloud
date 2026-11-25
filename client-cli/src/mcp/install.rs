//! `nordkraft mcp install`: put the server into the agent configs found on
//! this machine. `nordkraft mcp doctor`: setup, tunnel, API and those
//! configs, checked in one go.

use std::path::PathBuf;

use colored::*;
use serde_json::{json, Value};

use crate::core::api::API_BASE_URL;
use crate::core::config::{get_connection_config_path, load_connection_config};

/// An MCP client whose config we know how to edit.
pub(crate) struct Client {
    /// Name as the user knows it.
    pub(crate) name: &'static str,
    /// What `--client` accepts.
    pub(crate) slug: &'static str,
    /// Its config file; `mcpServers` lives at the top level in all of them.
    pub(crate) path: PathBuf,
    /// A directory whose presence means the client is installed, even
    /// before it has written a config file.
    pub(crate) marker: PathBuf,
}

impl Client {
    pub(crate) fn detected(&self) -> bool {
        self.path.exists() || self.marker.exists()
    }
}

pub(crate) fn clients() -> Vec<Client> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let claude_desktop_dir = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Claude")
    } else {
        dirs::config_dir()
            .unwrap_or_else(|| home.join(".config"))
            .join("Claude")
    };
    vec![
        Client {
            name: "Claude Desktop",
            slug: "claude-desktop",
            path: claude_desktop_dir.join("claude_desktop_config.json"),
            marker: claude_desktop_dir,
        },
        Client {
            name: "Claude Code",
            slug: "claude-code",
            path: home.join(".claude.json"),
            marker: home.join(".claude"),
        },
        Client {
            name: "Cursor",
            slug: "cursor",
            path: home.join(".cursor/mcp.json"),
            marker: home.join(".cursor"),
        },
    ]
}

/// The binary agents should start. GUI apps on macOS do not see the shell's
/// PATH (no /usr/local/bin), so the absolute path of the binary running
/// right now is used. `nordkraft update` replaces it in place.
pub(crate) fn binary_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "nordkraft".to_string())
}

/// The `mcpServers.nordkraft` entry.
pub(crate) fn server_entry() -> Value {
    json!({"command": binary_path(), "args": ["mcp"]})
}

/// The snippet for a manual setup.
pub(crate) fn snippet() -> String {
    serde_json::to_string_pretty(&json!({"mcpServers": {"nordkraft": server_entry()}}))
        .unwrap_or_default()
}

/// Read a client's config, or an empty object when the file is missing.
fn read_config(path: &PathBuf) -> Result<Value, Box<dyn std::error::Error>> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {}", path.display(), e))?;
    if !value.is_object() {
        return Err(format!("{} does not hold a JSON object", path.display()).into());
    }
    Ok(value)
}

/// What `install` did for one client.
#[derive(Debug, PartialEq)]
enum Outcome {
    Written,
    Unchanged,
    Skipped,
}

/// Put the entry into one client config. A `.bak` copy is left beside the
/// file on the first write.
fn install_into(client: &Client, entry: &Value) -> Result<Outcome, Box<dyn std::error::Error>> {
    let mut config = read_config(&client.path)?;
    let servers = config
        .as_object_mut()
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        return Err(format!("{}: mcpServers is not an object", client.path.display()).into());
    }
    if servers.get("nordkraft") == Some(entry) {
        return Ok(Outcome::Unchanged);
    }
    servers
        .as_object_mut()
        .unwrap()
        .insert("nordkraft".to_string(), entry.clone());

    if let Some(dir) = client.path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if client.path.exists() {
        let bak = client.path.with_extension("json.bak");
        std::fs::copy(&client.path, &bak)?;
    }
    std::fs::write(
        &client.path,
        format!("{}\n", serde_json::to_string_pretty(&config)?),
    )?;
    Ok(Outcome::Written)
}

/// `nordkraft mcp install [--client X] [--print]`
pub(crate) async fn install(
    only: Option<String>,
    print_only: bool,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let entry = server_entry();

    if print_only {
        if json_output {
            println!("{}", json!({"mcpServers": {"nordkraft": entry}}));
        } else {
            println!("{}", snippet());
        }
        return Ok(());
    }

    let all = clients();
    let targets: Vec<&Client> = match &only {
        Some(slug) => {
            let found = all
                .iter()
                .find(|c| c.slug == slug.as_str())
                .ok_or_else(|| {
                    format!(
                        "Unknown client '{}'. Known: {}",
                        slug,
                        all.iter().map(|c| c.slug).collect::<Vec<_>>().join(", ")
                    )
                })?;
            vec![found]
        }
        None => all.iter().filter(|c| c.detected()).collect(),
    };

    let mut report = Vec::new();
    for client in &all {
        let outcome = if targets.iter().any(|t| t.slug == client.slug) {
            install_into(client, &entry)?
        } else {
            Outcome::Skipped
        };
        report.push((client, outcome));
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "binary": binary_path(),
                "entry": entry,
                "clients": report.iter().map(|(c, o)| json!({
                    "name": c.name,
                    "client": c.slug,
                    "path": c.path,
                    "status": format!("{:?}", o).to_lowercase(),
                })).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    let written: Vec<_> = report
        .iter()
        .filter(|(_, o)| *o != Outcome::Skipped)
        .collect();
    if written.is_empty() {
        println!("{}", "No MCP client found on this machine.".yellow());
        println!("   Looked for Claude Desktop, Claude Code and Cursor.");
        println!("   Add this to your agent's MCP config by hand:");
        println!();
        println!("{}", snippet());
        return Ok(());
    }

    println!("{}", "🤖 Nordkraft MCP server".cyan().bold());
    println!("   {} {}", "Binary:".dimmed(), binary_path());
    println!();
    for (client, outcome) in &written {
        match outcome {
            Outcome::Written => println!(
                "   {} {}: {}",
                "✓".green(),
                client.name.white().bold(),
                client.path.display().to_string().dimmed()
            ),
            Outcome::Unchanged => println!(
                "   {} {}: already configured {}",
                "✓".green(),
                client.name.white().bold(),
                format!("({})", client.path.display()).dimmed()
            ),
            Outcome::Skipped => {}
        }
    }
    println!();
    println!(
        "   {} Restart the app (or start a new Claude Code session) to pick it up.",
        "→".dimmed()
    );
    println!(
        "   {} Then try: \"list my Nordkraft containers\"",
        "→".dimmed()
    );
    println!();
    println!("   {}", "For any other client, the entry is:".dimmed());
    for line in snippet().lines() {
        println!("   {}", line.dimmed());
    }
    Ok(())
}

/// One doctor check.
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
    hint: Option<String>,
}

/// `nordkraft mcp doctor`
pub(crate) async fn doctor(json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut checks = Vec::new();

    // 1. setup done?
    let conn = load_connection_config();
    checks.push(Check {
        name: "setup",
        ok: conn.is_some(),
        detail: match &conn {
            Some(c) => format!("{} ({})", c.email, get_connection_config_path().display()),
            None => format!("{} missing", get_connection_config_path().display()),
        },
        hint: conn.is_none().then(|| {
            "Run: nordkraft setup NKINVITE-… (the token is in your welcome mail)".to_string()
        }),
    });

    // 2. API reachable over the tunnel?
    let api_url = API_BASE_URL.clone();
    let api = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()?
        .get(format!("{}/status", api_url))
        .send()
        .await;
    let (api_ok, api_detail) = match api {
        Ok(r) if r.status().is_success() => {
            let body: Value = r.json().await.unwrap_or(Value::Null);
            let version = body["version"].as_str().unwrap_or("?");
            (
                true,
                format!("{} answers (controller {})", api_url, version),
            )
        }
        Ok(r) => (false, format!("{} answered HTTP {}", api_url, r.status())),
        Err(e) if e.is_connect() || e.is_timeout() => (false, format!("{} unreachable", api_url)),
        Err(e) => (false, format!("{}: {}", api_url, e)),
    };
    checks.push(Check {
        name: "api",
        ok: api_ok,
        detail: api_detail,
        hint: (!api_ok).then(|| "Is WireGuard connected? Run: nordkraft connect".to_string()),
    });

    // 3. the binary agents will start
    let binary = binary_path();
    checks.push(Check {
        name: "binary",
        ok: true,
        detail: binary.clone(),
        hint: None,
    });

    // 4. each client config
    let entry = server_entry();
    let mut installed_anywhere = false;
    for client in clients() {
        if !client.detected() {
            checks.push(Check {
                name: client.name,
                ok: true,
                detail: "not installed on this machine".to_string(),
                hint: None,
            });
            continue;
        }
        let (ok, detail, hint) = match read_config(&client.path) {
            Ok(config) => match config.get("mcpServers").and_then(|s| s.get("nordkraft")) {
                Some(found) if found == &entry => {
                    installed_anywhere = true;
                    (
                        true,
                        format!("configured ({})", client.path.display()),
                        None,
                    )
                }
                Some(found) => {
                    installed_anywhere = true;
                    (
                        false,
                        format!(
                            "configured, but points at {} ({})",
                            found["command"].as_str().unwrap_or("?"),
                            client.path.display()
                        ),
                        Some(format!(
                            "Run: nordkraft mcp install --client {}",
                            client.slug
                        )),
                    )
                }
                None => (
                    false,
                    format!("no nordkraft entry in {}", client.path.display()),
                    Some(format!(
                        "Run: nordkraft mcp install --client {}",
                        client.slug
                    )),
                ),
            },
            Err(e) => (false, e.to_string(), None),
        };
        checks.push(Check {
            name: client.name,
            ok,
            detail,
            hint,
        });
    }

    let failed = checks.iter().filter(|c| !c.ok).count();

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": failed == 0,
                "installed_in_any_client": installed_anywhere,
                "checks": checks.iter().map(|c| json!({
                    "name": c.name, "ok": c.ok, "detail": c.detail, "hint": c.hint
                })).collect::<Vec<_>>(),
            }))?
        );
    } else {
        println!("{}", "🩺 Nordkraft MCP doctor".cyan().bold());
        println!();
        for c in &checks {
            let mark = if c.ok { "✓".green() } else { "✗".red() };
            println!("   {} {:<16} {}", mark, c.name, c.detail);
            if let Some(hint) = &c.hint {
                println!("     {}", hint.yellow());
            }
        }
        println!();
        if failed == 0 {
            println!(
                "{}",
                "   All good. Ask your agent to list your containers.".green()
            );
        } else {
            println!(
                "{}",
                format!("   {} check(s) need attention.", failed).yellow()
            );
        }
    }

    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

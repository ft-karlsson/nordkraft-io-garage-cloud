// volume.rs — `nordkraft backup` / `nordkraft restore` / `deploy --volume-from`.
//
// A backup is the container's volume streamed as tar.zst to a file on this
// machine. A restore streams such a file back into a container's volume.
// Nothing is kept on the platform.

use crate::{get_container_or_select, non_interactive, resolve_alias, ApiError, API_BASE_URL};
use colored::*;
use dialoguer::{theme::ColorfulTheme, Confirm};
use futures::StreamExt;
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

const MANIFEST_HEADER: &str = "X-Nordkraft-Manifest";
const SHA256_HEADER: &str = "X-Nordkraft-Sha256";
const PROGRESS_STEP: u64 = 1024 * 1024;

type CliResult<T> = Result<T, Box<dyn std::error::Error>>;

/// No overall timeout: a transfer takes as long as it takes.
pub(crate) fn transfer_client() -> CliResult<Client> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()?)
}

pub(crate) fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// `<name>-<UTC yyyymmddThhmm>.tar.zst`, with anything odd in the name
/// replaced so the file is safe to create anywhere.
pub(crate) fn default_backup_name(container: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let safe: String = container
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_matches('.').to_string();
    let safe = if safe.is_empty() {
        "volume".to_string()
    } else {
        safe
    };
    format!("{}-{}.tar.zst", safe, now.format("%Y%m%dT%H%M"))
}

async fn api_error(resp: reqwest::Response) -> Box<dyn std::error::Error> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    match serde_json::from_str::<ApiError>(&text) {
        Ok(e) => e.error.into(),
        Err(_) if !text.trim().is_empty() => format!("{}: {}", status, text.trim()).into(),
        Err(_) => format!("HTTP {status}").into(),
    }
}

fn progress(prefix: &str, done: u64, total: Option<u64>) {
    let mut err = std::io::stderr();
    match total {
        Some(t) if t > 0 => {
            let _ = write!(
                err,
                "\r   {prefix} {} / {} ",
                human_bytes(done),
                human_bytes(t)
            );
        }
        _ => {
            let _ = write!(err, "\r   {prefix} {} ", human_bytes(done));
        }
    }
    let _ = err.flush();
}

// ---------------------------------------------------------------------------
// backup
// ---------------------------------------------------------------------------

pub(crate) async fn handle_backup(
    container: Option<String>,
    output: Option<String>,
    json_output: bool,
) -> CliResult<()> {
    let typed = get_container_or_select(container, json_output).await?;
    let container = resolve_alias(&typed);
    let to_stdout = output.as_deref() == Some("-");
    let quiet = json_output || to_stdout;

    let client = transfer_client()?;
    let url = format!("{}/containers/{}/volume", *API_BASE_URL, container);
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        return Err(api_error(resp).await);
    }

    let manifest: serde_json::Value = resp
        .headers()
        .get(MANIFEST_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);
    let consistency = manifest["consistency"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let volume_path = manifest["volume_path"].as_str().unwrap_or("?").to_string();
    let volume_bytes = manifest["volume_bytes"].as_u64().unwrap_or(0);

    if !quiet {
        println!(
            "{}",
            format!(
                "📦 {} ({}) — {}, {}",
                typed,
                volume_path,
                consistency,
                human_bytes(volume_bytes)
            )
            .cyan()
        );
        if consistency == "live" {
            eprintln!(
                "   {} live copy — stop the container first for a consistent backup of a database",
                "⚠".yellow()
            );
        }
    }

    let mut stream = resp.bytes_stream();
    let mut received: u64 = 0;

    if to_stdout {
        let mut out = tokio::io::stdout();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("transfer failed: {e}"))?;
            out.write_all(&chunk).await?;
        }
        out.flush().await?;
        return Ok(());
    }

    let final_path =
        PathBuf::from(output.unwrap_or_else(|| default_backup_name(&typed, chrono::Utc::now())));
    let part_path = PathBuf::from(format!("{}.part", final_path.display()));
    let mut file = tokio::fs::File::create(&part_path).await?;
    let mut last_shown = 0u64;
    let result: CliResult<()> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("transfer failed: {e}"))?;
            received += chunk.len() as u64;
            file.write_all(&chunk).await?;
            if !quiet && received - last_shown >= PROGRESS_STEP {
                last_shown = received;
                progress("📥", received, None);
            }
        }
        file.flush().await?;
        file.sync_all().await?;
        Ok(())
    }
    .await;
    drop(file);

    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&part_path).await;
        return Err(e);
    }
    if received == 0 {
        let _ = tokio::fs::remove_file(&part_path).await;
        return Err("the node sent an empty archive".into());
    }
    tokio::fs::rename(&part_path, &final_path).await?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "container": container,
                "file": final_path.display().to_string(),
                "bytes": received,
                "manifest": manifest,
            }))?
        );
    } else {
        eprint!("\r{}\r", " ".repeat(60));
        println!(
            "{} {} ({})",
            "✅".green().bold(),
            final_path.display().to_string().white().bold(),
            human_bytes(received)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// restore
// ---------------------------------------------------------------------------

pub(crate) async fn handle_restore(
    container: Option<String>,
    file: String,
    yes: bool,
    json_output: bool,
) -> CliResult<()> {
    let typed = get_container_or_select(container, json_output).await?;
    let container = resolve_alias(&typed);
    let path = Path::new(&file);
    if !path.is_file() {
        return Err(format!("{} is not a file", path.display()).into());
    }

    if !yes && !non_interactive() {
        let ok = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "This replaces everything in the volume of {typed}. Continue?"
            ))
            .default(false)
            .interact()?;
        if !ok {
            println!("{}", "Cancelled.".yellow());
            return Ok(());
        }
    } else if !yes && non_interactive() {
        return Err("restore overwrites data; pass --yes to confirm".into());
    }

    let report = restore_file(&container, path, json_output).await?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_restore_summary(&report);
    }
    Ok(())
}

pub(crate) fn print_restore_summary(report: &serde_json::Value) {
    eprint!("\r{}\r", " ".repeat(60));
    let bytes = report["received_bytes"].as_u64().unwrap_or(0);
    let restarted = report["restarted"].as_bool().unwrap_or(false);
    println!(
        "{} restored {} into {}{}",
        "✅".green().bold(),
        human_bytes(bytes),
        report["volume_path"].as_str().unwrap_or("the volume"),
        if restarted { ", container started" } else { "" }
    );
    if let Some(ws) = report["warnings"].as_array() {
        for w in ws {
            if let Some(w) = w.as_str() {
                println!("   {} {}", "⚠".yellow(), w);
            }
        }
    }
}

/// Stream `path` into the container's volume. Returns the node's report.
pub(crate) async fn restore_file(
    container: &str,
    path: &Path,
    quiet: bool,
) -> CliResult<serde_json::Value> {
    let len = tokio::fs::metadata(path).await?.len();
    if len == 0 {
        return Err(format!("{} is empty", path.display()).into());
    }

    if !quiet {
        println!(
            "{}",
            format!(
                "📤 {} ({}) → {}",
                path.display(),
                human_bytes(len),
                container
            )
            .cyan()
        );
    }
    let sha = sha256_of(path).await?;

    let file = tokio::fs::File::open(path).await?;
    let mut sent: u64 = 0;
    let mut last_shown = 0u64;
    let stream = ReaderStream::with_capacity(file, 256 * 1024).map(move |chunk| {
        if let Ok(b) = &chunk {
            sent += b.len() as u64;
            if !quiet && sent - last_shown >= PROGRESS_STEP {
                last_shown = sent;
                progress("📤", sent, Some(len));
            }
        }
        chunk
    });

    let client = transfer_client()?;
    let url = format!("{}/containers/{}/volume", *API_BASE_URL, container);
    let resp = client
        .put(&url)
        .header(reqwest::header::CONTENT_LENGTH, len)
        .header(reqwest::header::CONTENT_TYPE, "application/zstd")
        .header(SHA256_HEADER, &sha)
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(api_error(resp).await);
    }
    Ok(resp.json().await?)
}

async fn sha256_of(path: &Path) -> CliResult<String> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_name_is_safe_and_dated() {
        let t = chrono::DateTime::parse_from_rfc3339("2026-10-26T18:40:11Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            default_backup_name("campfire", t),
            "campfire-20261026T1840.tar.zst"
        );
        assert_eq!(
            default_backup_name("app-62ad156b-df6f", t),
            "app-62ad156b-df6f-20261026T1840.tar.zst"
        );
        assert_eq!(
            default_backup_name("../x y", t),
            "_x_y-20261026T1840.tar.zst"
        );
        assert_eq!(default_backup_name("", t), "volume-20261026T1840.tar.zst");
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(9_600_000), "9.6 MB");
        assert_eq!(human_bytes(2_500_000_000), "2.5 GB");
    }
}

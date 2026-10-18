// stream.rs — `nordkraft shell` and `nordkraft exec`.
//
// `shell` opens GET /containers/<name>/stream as a WebSocket and wires it to
// the local terminal: raw mode, stdin bytes → channel 0, channel 1/2 → the
// screen, SIGWINCH → a resize frame, and the status frame → our exit code.
// `exec` is the buffered POST — output is printed as-is and the process's
// exit code becomes ours, so scripts can `nordkraft exec web -- test -f x`.
//
// Framing is the Kubernetes v4 channel protocol: first byte of every binary
// frame is the channel (0 stdin, 1 stdout, 2 stderr, 3 status, 4 resize).

use futures::{SinkExt, StreamExt};
use std::io::{Read, Write};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const STDIN: u8 = 0;
const STDOUT: u8 = 1;
const STDERR: u8 = 2;
const STATUS: u8 = 3;
const RESIZE: u8 = 4;

type Error = Box<dyn std::error::Error>;

/// Interactive shell. Returns the remote exit code.
pub async fn shell(container: &str, cmd: &[String]) -> Result<i32, Error> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut query = vec![
        ("tty".to_string(), "true".to_string()),
        ("cols".to_string(), cols.to_string()),
        ("rows".to_string(), rows.to_string()),
    ];
    let cmd = if cmd.is_empty() {
        &["/bin/sh".to_string()][..]
    } else {
        cmd
    };
    for c in cmd {
        query.push(("cmd".to_string(), c.clone()));
    }
    let url = stream_url(container, &query);

    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| format!("{e}\n  (is WireGuard up, and is {container} running?)"))?;
    let (mut sink, mut source) = ws.split();

    // Raw mode for the whole session; a guard restores it on every exit path,
    // including a panic, so the user never gets a wedged terminal.
    crossterm::terminal::enable_raw_mode()?;
    let _restore = RawModeGuard;

    // stdin is read on a blocking thread — tokio's stdin is line-buffered
    // through a blocking pool anyway, and this way ^C reaches the container.
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut stdout = std::io::stdout().lock();
    let mut exit_code: Option<i32> = None;
    let mut remote_error: Option<String> = None;

    loop {
        tokio::select! {
            msg = source.next() => match msg {
                Some(Ok(Message::Binary(frame))) => {
                    let Some((ch, payload)) = frame.split_first() else { continue };
                    match *ch {
                        STDOUT | STDERR => {
                            stdout.write_all(payload)?;
                            stdout.flush()?;
                        }
                        STATUS => {
                            let v: serde_json::Value = serde_json::from_slice(payload).unwrap_or_default();
                            exit_code = v["exit_code"].as_i64().map(|c| c as i32);
                            remote_error = v["error"].as_str().map(str::to_string);
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => { remote_error.get_or_insert(e.to_string()); break; }
            },
            input = stdin_rx.recv() => match input {
                Some(bytes) => {
                    let mut frame = Vec::with_capacity(bytes.len() + 1);
                    frame.push(STDIN);
                    frame.extend_from_slice(&bytes);
                    if sink.send(Message::Binary(frame)).await.is_err() { break; }
                }
                None => {
                    // Local stdin closed (piped input ended): send EOF.
                    let _ = sink.send(Message::Binary(vec![STDIN])).await;
                }
            },
            _ = winch.recv() => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    let payload = serde_json::json!({"width": cols, "height": rows}).to_string();
                    let mut frame = vec![RESIZE];
                    frame.extend_from_slice(payload.as_bytes());
                    let _ = sink.send(Message::Binary(frame)).await;
                }
            }
        }
    }
    drop(_restore);

    if let Some(err) = remote_error {
        eprintln!("\r\nnordkraft shell: {err}");
        return Ok(exit_code.unwrap_or(1));
    }
    Ok(exit_code.unwrap_or(0))
}

struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[allow(clippy::too_many_arguments)]
/// Buffered exec. Prints the process's stdout/stderr to ours and returns
/// its exit code (or 124 on timeout, like coreutils `timeout`).
pub async fn exec(
    container: &str,
    cmd: &[String],
    cwd: Option<&str>,
    env: &[String],
    user: Option<&str>,
    timeout_s: u64,
    stdin_data: Option<Vec<u8>>,
    json_output: bool,
) -> Result<i32, Error> {
    let env_map: serde_json::Map<String, serde_json::Value> = env
        .iter()
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
        .collect();
    let body = serde_json::json!({
        "cmd": cmd,
        "cwd": cwd,
        "env": env_map,
        "user": user,
        "timeout_s": timeout_s,
        "stdin": stdin_data.map(|b| String::from_utf8_lossy(&b).into_owned()),
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_s + 30))
        .build()?;
    let url = format!("{}/containers/{}/exec", *crate::API_BASE_URL, container);
    let resp = client.post(&url).json(&body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("{status}: {text}").into());
    }
    let result: serde_json::Value = resp.json().await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        if let Some(out) = result["stdout"].as_str() {
            print!("{out}");
        }
        if let Some(err) = result["stderr"].as_str() {
            eprint!("{err}");
        }
        if result["truncated"].as_bool() == Some(true) {
            eprintln!("nordkraft exec: output truncated at 1 MiB");
        }
        if let Some(e) = result["error"].as_str() {
            eprintln!("nordkraft exec: {e}");
        }
    }
    Ok(match result["exit_code"].as_i64() {
        Some(c) => c as i32,
        None if result["error"].as_str().unwrap_or("").contains("timeout") => 124,
        None => 1,
    })
}

fn stream_url(container: &str, query: &[(String, String)]) -> String {
    let base = crate::API_BASE_URL
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let qs = query
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}/containers/{container}/stream?{qs}")
}

/// Block until the container's main process exits. Exit code 124 on timeout.
pub async fn wait(container: &str, timeout_s: u64, json_output: bool) -> Result<i32, Error> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_s + 30))
        .build()?;
    let url = format!(
        "{}/containers/{}/wait?timeout_s={}",
        *crate::API_BASE_URL,
        container,
        timeout_s.max(1)
    );
    let resp = client.post(&url).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("{status}: {text}").into());
    }
    let result: serde_json::Value = resp.json().await?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    if result["timed_out"].as_bool() == Some(true) {
        if !json_output {
            eprintln!("nordkraft wait: still running after {timeout_s}s");
        }
        return Ok(124);
    }
    match result["exit_code"].as_i64() {
        Some(c) => Ok(c as i32),
        None => {
            if !json_output {
                eprintln!("nordkraft wait: container has no running process");
            }
            Ok(1)
        }
    }
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

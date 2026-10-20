// stream/session.rs — drive one exec process over a framed byte stream.
//
// The transport is deliberately abstract: two channels of binary frames.
// The route layer (rocket_ws on the agent, or the same route in hybrid
// mode) pumps WebSocket messages into/out of them; this file never sees a
// socket. That keeps the kernel of the feature testable without a server.
//
// Lifecycle of a session, in order:
//   1. spawn the process (fails → a single status frame with `error`, done)
//   2. pump: client frames → stdin/resize, process output → client frames
//   3. the process exits, or the client goes away, or idle/max time is hit
//   4. exactly one status frame, then the outgoing channel closes
// Whatever ends the session, the process is reaped — no orphan shells.

use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::containerd_exec::{ContainerdExec, ExecSpec, StartError};
use super::protocol::{self, Resize, Status};

/// Frames from the client (already stripped of WebSocket envelope).
pub type Incoming = mpsc::Receiver<Vec<u8>>;
/// Frames to the client. Dropping it closes the connection.
pub type Outgoing = mpsc::Sender<Vec<u8>>;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// No frames in either direction for this long → session ends.
    pub idle: Duration,
    /// Hard cap on session length.
    pub max: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(30 * 60),
            max: Duration::from_secs(12 * 60 * 60),
        }
    }
}

/// Run a session to completion. Returns the exit code when the process
/// exited normally.
pub async fn run(
    exec: &ContainerdExec,
    spec: ExecSpec,
    mut incoming: Incoming,
    outgoing: Outgoing,
    limits: Limits,
) -> Option<i32> {
    let mut proc = match exec.spawn(spec).await {
        Ok(p) => p,
        Err(e) => {
            // A command the user's own shell would have refused ("not found",
            // "permission denied") is the user's result, not a node problem:
            // deliver it exactly as sh would — message on stderr, exit 127/126
            // — and keep it out of the node's log beyond debug.
            if let Some(se) = e.downcast_ref::<StartError>() {
                if let Some(code) = se.exit_code() {
                    debug!("exec refused by runtime: {se}");
                    let _ = outgoing
                        .send(protocol::frame(
                            protocol::STDERR,
                            format!("{se}\n").as_bytes(),
                        ))
                        .await;
                    let _ = outgoing
                        .send(protocol::status_frame(&Status::exited(code)))
                        .await;
                    return Some(code);
                }
            }
            warn!("exec spawn failed: {e}");
            let _ = outgoing
                .send(protocol::status_frame(&Status::failed(e.to_string())))
                .await;
            return None;
        }
    };
    let exec_id = proc.exec_id.clone();
    let container_id = proc.container_id.clone();
    debug!("session {exec_id}: started");

    let started = Instant::now();
    let mut last_activity = Instant::now();
    let mut idle_tick = tokio::time::interval(Duration::from_secs(1));
    idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Own the only stdin sender, so dropping it is the container's EOF.
    let mut stdin = proc.stdin.take();

    let outcome: Outcome = loop {
        tokio::select! {
            frame = incoming.recv() => match frame {
                None => break Outcome::ClientGone,
                Some(data) => {
                    last_activity = Instant::now();
                    match protocol::unframe(&data) {
                        Some((protocol::STDIN, payload)) => {
                            if payload.is_empty() {
                                // Zero-length stdin frame = EOF (what kubectl sends on ^D with no tty).
                                stdin.take();
                            } else if let Some(tx) = &stdin {
                                if tx.send(payload.to_vec()).await.is_err() {
                                    stdin.take();
                                }
                            }
                        }
                        Some((protocol::RESIZE, payload)) => {
                            if let Ok(r) = serde_json::from_slice::<Resize>(payload) {
                                let _ = proc.resize(r).await;
                            }
                        }
                        _ => {} // unknown channel from client: ignore, never fatal
                    }
                }
            },
            out = proc.output.recv() => match out {
                Some((ch, bytes)) => {
                    last_activity = Instant::now();
                    if outgoing.send(protocol::frame(ch, &bytes)).await.is_err() {
                        break Outcome::ClientGone;
                    }
                }
                None => {
                    // Both reader FIFOs hit EOF: the process is gone.
                    break Outcome::Exited;
                }
            },
            _ = idle_tick.tick() => {
                // A live session keeps the container "in use" for auto_stop.
                exec.touch(&container_id);
                if last_activity.elapsed() > limits.idle {
                    break Outcome::Idle;
                }
                if started.elapsed() > limits.max {
                    break Outcome::MaxTime;
                }
            }
        }
    };
    drop(stdin);

    let status = match outcome {
        Outcome::Exited => match proc.wait().await {
            Ok(code) => {
                proc.finish().await;
                Status::exited(code)
            }
            Err(e) => {
                proc.abort().await;
                Status::failed(format!("wait: {e}"))
            }
        },
        Outcome::ClientGone => {
            debug!("session {exec_id}: client gone, killing process");
            proc.abort().await;
            return None;
        }
        Outcome::Idle => {
            proc.abort().await;
            Status::failed("session idle timeout")
        }
        Outcome::MaxTime => {
            proc.abort().await;
            Status::failed("session max duration reached")
        }
    };
    debug!("session {exec_id}: {status:?}");
    let code = status.exit_code;
    let _ = outgoing.send(protocol::status_frame(&status)).await;
    code
}

enum Outcome {
    Exited,
    ClientGone,
    Idle,
    MaxTime,
}

/// Buffered, non-interactive exec: run to completion and collect output.
/// This is what `POST /containers/<id>/exec` returns to Mastra-style callers.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExecResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u128,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Drive a non-interactive session over its two channels and collect the
/// result. Works the same whether the far end is an in-process `run` or a
/// relayed agent stream: feed stdin, send EOF, gather stdout/stderr, keep
/// the status frame. `to_session` stays open until the session closes
/// `from_session`, so the far end never sees a premature "client gone".
pub async fn collect(
    to_session: mpsc::Sender<Vec<u8>>,
    mut from_session: mpsc::Receiver<Vec<u8>>,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    max_bytes: usize,
) -> ExecResult {
    let t0 = Instant::now();
    if let Some(data) = stdin {
        let _ = to_session
            .send(protocol::frame(protocol::STDIN, &data))
            .await;
    }
    let _ = to_session.send(protocol::frame(protocol::STDIN, &[])).await;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut truncated = false;
    let mut status: Option<Status> = None;

    // The session enforces `timeout` itself; this is the belt to that brace.
    let deadline = tokio::time::sleep(timeout + Duration::from_secs(15));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            frame = from_session.recv() => match frame {
                None => break,
                Some(data) => match protocol::unframe(&data) {
                    Some((protocol::STDOUT, p)) => append_capped(&mut stdout, p, max_bytes, &mut truncated),
                    Some((protocol::STDERR, p)) => append_capped(&mut stderr, p, max_bytes, &mut truncated),
                    Some((protocol::STATUS, p)) => status = serde_json::from_slice(p).ok(),
                    _ => {}
                },
            },
            _ = &mut deadline => {
                status.get_or_insert(Status::failed("exec did not finish in time"));
                break;
            }
        }
    }
    drop(to_session);

    let status = status.unwrap_or_else(|| Status::failed("session ended without status"));
    ExecResult {
        exit_code: status.exit_code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        duration_ms: t0.elapsed().as_millis(),
        truncated,
        error: status.error,
    }
}

fn append_capped(buf: &mut Vec<u8>, chunk: &[u8], cap: usize, truncated: &mut bool) {
    let room = cap.saturating_sub(buf.len());
    if chunk.len() > room {
        buf.extend_from_slice(&chunk[..room]);
        *truncated = true;
    } else {
        buf.extend_from_slice(chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_marks_truncation() {
        let mut b = Vec::new();
        let mut t = false;
        append_capped(&mut b, b"hello", 3, &mut t);
        assert_eq!(b, b"hel");
        assert!(t);
    }
}

#[cfg(test)]
mod live {
    use super::*;
    use crate::stream::containerd_exec::ContainerdExec;

    fn spec(cmd: &[&str]) -> ExecSpec {
        ExecSpec {
            container: "web".into(),
            owner_pubkey: Some("TESTKEY".into()),
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            env: vec!["GREETING=hej".into()],
            cwd: Some("/tmp".into()),
            user: None,
            tty: false,
            initial_size: None,
        }
    }

    #[tokio::test]
    #[ignore]
    async fn live_collect_over_session_channels() {
        let exec = ContainerdExec::new("/run/containerd/containerd.sock", "default");
        let (to_tx, to_rx) = mpsc::channel(8);
        let (from_tx, from_rx) = mpsc::channel(8);
        tokio::spawn(async move {
            run(
                &exec,
                spec(&[
                    "/bin/sh",
                    "-c",
                    "pwd; echo $GREETING; cat; echo done >&2; exit 5",
                ]),
                to_rx,
                from_tx,
                Limits::default(),
            )
            .await
        });
        let r = collect(
            to_tx,
            from_rx,
            Some(b"from-stdin\n".to_vec()),
            Duration::from_secs(10),
            1 << 20,
        )
        .await;
        assert_eq!(r.exit_code, Some(5), "{r:?}");
        assert_eq!(r.stdout, "/tmp\nhej\nfrom-stdin\n");
        assert_eq!(r.stderr, "done\n");
        assert!(!r.truncated);
        assert!(r.error.is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn live_timeout_kills_process() {
        let exec = ContainerdExec::new("/run/containerd/containerd.sock", "default");
        let (to_tx, to_rx) = mpsc::channel(8);
        let (from_tx, from_rx) = mpsc::channel(8);
        tokio::spawn(async move {
            run(
                &exec,
                spec(&["/bin/sleep", "60"]),
                to_rx,
                from_tx,
                Limits {
                    idle: Duration::from_secs(2),
                    max: Duration::from_secs(2),
                },
            )
            .await
        });
        let t0 = Instant::now();
        let r = collect(to_tx, from_rx, None, Duration::from_secs(2), 1 << 20).await;
        assert!(r.exit_code.is_none());
        assert!(
            r.error.as_deref().unwrap_or("").contains("timeout"),
            "{r:?}"
        );
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "took {:?}",
            t0.elapsed()
        );
    }
}

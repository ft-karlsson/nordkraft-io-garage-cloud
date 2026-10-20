// exec.rs — run a process in this sandbox: buffered (POST /v1/exec) or as a
// framed stream (GET /v1/stream, WebSocket). Both spawn the same way; the
// stream adds stdin, a pty and resize.
//
// Everything a client could not get reliably through the runtime's exec
// path is trivial here, because we are the process's parent: stdin EOF is
// `drop(stdin)`, a terminal is `openpty`, resize is an ioctl, and output
// is read until the pipe closes — nothing in between can lose it.

use crate::init::Tracked;
use crate::protocol::{self, Resize, Status};
use nix::pty::{openpty, OpenptyResult, Winsize};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub const EXEC_DEFAULT_TIMEOUT: u64 = 120;
pub const EXEC_MAX_TIMEOUT: u64 = 3600;
pub const EXEC_OUTPUT_CAP: usize = 1024 * 1024;
const READ_CHUNK: usize = 32 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct Spec {
    pub cmd: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// "uid" or "uid:gid" — numeric only.
    #[serde(default)]
    pub user: Option<String>,
}

/// `own_group`: put the child in its own process group so a timeout or kill
/// takes the whole tree. A pty child skips this — its `pre_exec` calls
/// `setsid`, which both creates the group and fails (EPERM) on a process
/// that is already a group leader.
fn apply_common(cmd: &mut Command, spec: &Spec, own_group: bool) -> Result<(), String> {
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(user) = &spec.user {
        let mut parts = user.splitn(2, ':');
        let uid = parts
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .ok_or_else(|| format!("user must be numeric uid[:gid], got {user:?}"))?;
        let gid = parts.next().and_then(|s| s.parse::<u32>().ok());
        cmd.uid(uid);
        cmd.gid(gid.unwrap_or(uid));
    }
    if own_group {
        cmd.process_group(0);
    }
    Ok(())
}

fn kill_group(child: &Child) {
    if let Some(pid) = child.id() {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Buffered exec
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    #[serde(flatten)]
    pub spec: Spec,
    #[serde(default)]
    pub stdin: Option<String>,
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ExecResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u128,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn run_buffered(req: ExecRequest, tracked: Tracked) -> ExecResult {
    let t0 = Instant::now();
    let fail = |msg: String| ExecResult {
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        duration_ms: t0.elapsed().as_millis(),
        truncated: false,
        error: Some(msg),
    };
    if req.spec.cmd.is_empty() {
        return fail("cmd required".into());
    }
    let timeout = Duration::from_secs(
        req.timeout_s
            .unwrap_or(EXEC_DEFAULT_TIMEOUT)
            .clamp(1, EXEC_MAX_TIMEOUT),
    );
    let mut cmd = Command::new(&req.spec.cmd[0]);
    cmd.args(&req.spec.cmd[1..]);
    if let Err(e) = apply_common(&mut cmd, &req.spec, true) {
        return fail(e);
    }
    // No stdin in the request → /dev/null, so `cat`-style commands see EOF at once.
    cmd.stdin(if req.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail(format!("spawn {}: {e}", req.spec.cmd[0])),
    };
    let pid = child.id().map(|p| p as i32).unwrap_or(0);
    tracked.add(pid);

    if let (Some(data), Some(mut stdin)) = (req.stdin, child.stdin.take()) {
        tokio::spawn(async move {
            let _ = stdin.write_all(data.as_bytes()).await;
            // Dropping the pipe is EOF.
        });
    }
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut truncated = false;
    let read_all = async {
        let (a, b) = tokio::join!(
            read_capped(&mut stdout, &mut out, EXEC_OUTPUT_CAP),
            read_capped(&mut stderr, &mut err, EXEC_OUTPUT_CAP)
        );
        a || b
    };
    let result = tokio::time::timeout(timeout, async {
        let t = read_all.await;
        let status = child.wait().await;
        (t, status)
    })
    .await;
    tracked.remove(pid);
    match result {
        Ok((t, Ok(status))) => {
            truncated |= t;
            ExecResult {
                exit_code: Some(exit_code(status)),
                stdout: String::from_utf8_lossy(&out).into_owned(),
                stderr: String::from_utf8_lossy(&err).into_owned(),
                duration_ms: t0.elapsed().as_millis(),
                truncated,
                error: None,
            }
        }
        Ok((_, Err(e))) => fail(format!("wait: {e}")),
        Err(_) => {
            kill_group(&child);
            let _ = child.wait().await;
            ExecResult {
                exit_code: None,
                stdout: String::from_utf8_lossy(&out).into_owned(),
                stderr: String::from_utf8_lossy(&err).into_owned(),
                duration_ms: t0.elapsed().as_millis(),
                truncated,
                error: Some(format!("timeout after {}s", timeout.as_secs())),
            }
        }
    }
}

/// Read to EOF, keeping at most `cap` bytes. Returns whether anything was dropped.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> bool {
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut truncated = false;
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(buf.len());
                if n > room {
                    buf.extend_from_slice(&chunk[..room]);
                    truncated = true;
                } else {
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
    }
    truncated
}

// ---------------------------------------------------------------------------
// Streamed exec (frames over channels; the WebSocket layer is in server.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct StreamSpec {
    pub spec: Spec,
    pub tty: bool,
    /// Attach stdin. With `false` the process reads /dev/null.
    pub stdin: bool,
    pub size: Option<Resize>,
}

pub type Incoming = mpsc::Receiver<Vec<u8>>;
pub type Outgoing = mpsc::Sender<Vec<u8>>;

/// Run one process to completion over framed channels. Always ends with a
/// status frame. When the client goes away the process group is killed.
pub async fn run_stream(
    spec: StreamSpec,
    mut incoming: Incoming,
    outgoing: Outgoing,
    tracked: Tracked,
) {
    if spec.spec.cmd.is_empty() {
        let _ = outgoing
            .send(protocol::status_frame(&Status::failed("cmd required")))
            .await;
        return;
    }
    let mut cmd = Command::new(&spec.spec.cmd[0]);
    cmd.args(&spec.spec.cmd[1..]);
    if let Err(e) = apply_common(&mut cmd, &spec.spec, !spec.tty) {
        let _ = outgoing
            .send(protocol::status_frame(&Status::failed(e)))
            .await;
        return;
    }
    cmd.kill_on_drop(true);

    // Output channel: (channel byte, bytes). Readers hold the only senders, so
    // `out_rx` ends exactly when every output descriptor hit EOF.
    let (out_tx, mut out_rx) = mpsc::channel::<(u8, Vec<u8>)>(64);
    let mut pty_master: Option<OwnedFd> = None;
    let mut stdin_writer: Option<Box<dyn tokio::io::AsyncWrite + Unpin + Send>> = None;

    let child = if spec.tty {
        let size = spec.size.unwrap_or(Resize {
            width: 80,
            height: 24,
        });
        let OpenptyResult { master, slave } = match openpty(
            Some(&Winsize {
                ws_row: size.height,
                ws_col: size.width,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        ) {
            Ok(p) => p,
            Err(e) => {
                let _ = outgoing
                    .send(protocol::status_frame(&Status::failed(format!(
                        "openpty: {e}"
                    ))))
                    .await;
                return;
            }
        };
        if !spec.spec.env.contains_key("TERM") {
            cmd.env("TERM", "xterm-256color");
        }
        // The slave becomes the child's stdio and controlling terminal.
        let slave_fd = slave.into_raw_fd();
        cmd.stdin(unsafe { Stdio::from_raw_fd(nix::unistd::dup(slave_fd).unwrap_or(slave_fd)) });
        cmd.stdout(unsafe { Stdio::from_raw_fd(nix::unistd::dup(slave_fd).unwrap_or(slave_fd)) });
        cmd.stderr(unsafe { Stdio::from_raw_fd(slave_fd) });
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::from)?;
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn();
        // Our end of the terminal: one fd, read and written through tokio's File.
        let master_raw = master.as_raw_fd();
        let reader = unsafe {
            tokio::fs::File::from_raw_fd(nix::unistd::dup(master_raw).unwrap_or(master_raw))
        };
        let writer = unsafe {
            tokio::fs::File::from_raw_fd(nix::unistd::dup(master_raw).unwrap_or(master_raw))
        };
        spawn_reader(reader, protocol::STDOUT, out_tx.clone());
        stdin_writer = Some(Box::new(writer));
        pty_master = Some(master);
        child
    } else {
        cmd.stdin(if spec.stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.spawn().map(|mut child| {
            if let Some(out) = child.stdout.take() {
                spawn_reader(out, protocol::STDOUT, out_tx.clone());
            }
            if let Some(err) = child.stderr.take() {
                spawn_reader(err, protocol::STDERR, out_tx.clone());
            }
            if let Some(stdin) = child.stdin.take() {
                stdin_writer = Some(Box::new(stdin));
            }
            child
        })
    };
    // The Command owns the parent's copies of the child's stdio (the pty
    // slave included) until it is dropped. Drop it now, or the master never
    // reads EOF after the child exits and a terminal session never ends.
    drop(cmd);
    drop(out_tx);

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            let _ = outgoing
                .send(protocol::status_frame(&Status::failed(format!(
                    "spawn {}: {e}",
                    spec.spec.cmd[0]
                ))))
                .await;
            return;
        }
    };
    let pid = child.id().map(|p| p as i32).unwrap_or(0);
    tracked.add(pid);
    debug!("stream: pid {pid} started: {:?}", spec.spec.cmd);

    let mut output_done = false;
    let mut client_gone = false;
    // Pump until output is drained; the client may leave earlier.
    while !output_done {
        tokio::select! {
            frame = incoming.recv(), if !client_gone => match frame {
                None => {
                    client_gone = true;
                    kill_group(&child);
                }
                Some(data) => match protocol::unframe(&data) {
                    Some((protocol::STDIN, payload)) => {
                        if payload.is_empty() {
                            // EOF: for a pty send ^D, for a pipe close it.
                            if spec.tty {
                                if let Some(w) = stdin_writer.as_mut() { let _ = w.write_all(&[4]).await; }
                            } else {
                                stdin_writer.take();
                            }
                        } else if let Some(w) = stdin_writer.as_mut() {
                            if w.write_all(payload).await.is_err() { stdin_writer.take(); }
                        }
                    }
                    Some((protocol::RESIZE, payload)) => {
                        if let (Some(master), Ok(r)) = (&pty_master, serde_json::from_slice::<Resize>(payload)) {
                            resize(master, r);
                        }
                    }
                    _ => {}
                },
            },
            out = out_rx.recv() => match out {
                Some((ch, bytes)) => {
                    if outgoing.send(protocol::frame(ch, &bytes)).await.is_err() {
                        client_gone = true;
                        kill_group(&child);
                    }
                }
                None => output_done = true,
            },
        }
    }
    let status = match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(Ok(s)) => Status::exited(exit_code(s)),
        Ok(Err(e)) => Status::failed(format!("wait: {e}")),
        Err(_) => {
            kill_group(&child);
            let _ = child.wait().await;
            Status::failed("process did not exit after its output closed")
        }
    };
    tracked.remove(pid);
    debug!("stream: pid {pid} → {status:?}");
    if !client_gone {
        let _ = outgoing.send(protocol::status_frame(&status)).await;
    }
}

fn spawn_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut r: R,
    channel: u8,
    tx: mpsc::Sender<(u8, Vec<u8>)>,
) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match r.read(&mut buf).await {
                Ok(0) => break,
                // A pty master returns EIO once the slave side is closed: that is its EOF.
                Err(e) => {
                    if e.raw_os_error() != Some(libc::EIO) {
                        warn!("read: {e}");
                    }
                    break;
                }
                Ok(n) => {
                    if tx.send((channel, buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

fn resize(master: &OwnedFd, r: Resize) {
    let ws = Winsize {
        ws_row: r.height,
        ws_col: r.width,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
    }
}

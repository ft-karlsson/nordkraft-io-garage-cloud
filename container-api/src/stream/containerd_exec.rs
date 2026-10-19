// stream/containerd_exec.rs — run a process inside a container via containerd.
//
// This is the kubelet/CRI half of `kubectl exec`: we talk to containerd's
// Tasks service over its Unix socket (Task.Exec / Start / ResizePty / Wait /
// Kill / DeleteProcess) and move bytes through the FIFOs the shim attaches
// to the process. No nerdctl subprocess, no pty emulation on our side —
// the shim (runc or Kata) owns the terminal, exactly as it does for kubectl.
//
// One `ExecProcess` per session. Its channels are the whole interface:
// write to `stdin`, read `(channel, bytes)` from `output`, `resize`, `kill`,
// and `wait` for the exit code. Dropping the stdin sender sends EOF.

use containerd_client::services::v1::{
    containers_client::ContainersClient, tasks_client::TasksClient, CloseIoRequest,
    DeleteProcessRequest, ExecProcessRequest, GetContainerRequest, GetRequest, KillRequest,
    ListContainersRequest, ResizePtyRequest, StartRequest, WaitRequest,
};
use containerd_client::tonic::transport::Channel;
use containerd_client::tonic::Request;
use nix::sys::stat::Mode;
use nix::unistd::mkfifo;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex, OnceCell};
use tracing::{debug, warn};

use super::protocol::{Resize, STDERR, STDOUT};

const FIFO_ROOT: &str = "/run/nordkraft/exec";
const FIFO_OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const KILL_WAIT: Duration = Duration::from_secs(5);
const OUTPUT_CHUNK: usize = 32 * 1024;

pub type ExecError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone)]
pub struct ExecSpec {
    /// nerdctl container name, or a containerd container id.
    pub container: String,
    /// When set, the container must carry this `owner_pubkey` label.
    pub owner_pubkey: Option<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub cwd: Option<String>,
    /// "uid", "uid:gid" or "user" — only numeric forms are honoured.
    pub user: Option<String>,
    pub tty: bool,
    pub initial_size: Option<Resize>,
    /// Attach a stdin stream. `false` is kubectl's `exec` without `-i`: no
    /// stdin FIFO is created and the process reads EOF from `/dev/null` at
    /// once, so no `CloseIO` is ever issued for it. That matters on Kata:
    /// kata-agent (seen on 3.25 and 3.32) cancels the shim's pending stdout
    /// read when stdin is closed, and whatever the process prints after that
    /// point is lost. Callers that never write stdin should pass `false`.
    pub stdin: bool,
}

/// A running exec process. See module docs.
pub struct ExecProcess {
    pub exec_id: String,
    /// containerd id of the container the process runs in.
    pub container_id: String,
    /// The only writer to the process's stdin; `take()` it and drop for EOF.
    pub stdin: Option<mpsc::Sender<Vec<u8>>>,
    pub output: mpsc::Receiver<(u8, Vec<u8>)>,
    inner: Arc<ProcInner>,
}

struct ProcInner {
    tasks: TasksClient<Channel>,
    ns: String,
    container_id: String,
    exec_id: String,
    fifo_dir: PathBuf,
    tty: bool,
    exit: Mutex<Option<i32>>,
}

/// Lazily-connected containerd client, shared by every session on this node.
/// Also the node's memory of when each container was last used by a
/// session, which the lifecycle reaper reads for `auto_stop`.
pub struct ContainerdExec {
    socket: PathBuf,
    ns: String,
    channel: OnceCell<Channel>,
    activity: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

impl ContainerdExec {
    pub fn new(socket: impl Into<PathBuf>, namespace: impl Into<String>) -> Self {
        Self {
            socket: socket.into(),
            ns: namespace.into(),
            channel: OnceCell::new(),
            activity: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Record that a session used this container just now.
    pub fn touch(&self, container_id: &str) {
        if let Ok(mut m) = self.activity.lock() {
            m.insert(container_id.to_string(), std::time::Instant::now());
        }
    }

    /// Last session activity on this container since the node started.
    pub fn last_activity(&self, container_id: &str) -> Option<std::time::Instant> {
        self.activity.lock().ok()?.get(container_id).copied()
    }

    /// Drop activity records for containers that no longer exist.
    pub fn retain_activity(&self, live: &std::collections::HashSet<String>) {
        if let Ok(mut m) = self.activity.lock() {
            m.retain(|id, _| live.contains(id));
        }
    }

    async fn channel(&self) -> Result<Channel, ExecError> {
        let ch = self
            .channel
            .get_or_try_init(|| async {
                containerd_client::connect(&self.socket)
                    .await
                    .map_err(|e| format!("containerd socket {}: {e}", self.socket.display()))
            })
            .await?;
        Ok(ch.clone())
    }

    /// Resolve a nerdctl name or raw id to the containerd container id,
    /// enforcing the owner label when one is required.
    pub async fn resolve(
        &self,
        name_or_id: &str,
        owner_pubkey: Option<&str>,
    ) -> Result<(String, serde_json::Value), ExecError> {
        let mut containers = ContainersClient::new(self.channel().await?);

        // nerdctl labels every container it creates with nerdctl/name.
        let by_name = containers
            .list(with_ns(
                ListContainersRequest {
                    filters: vec![format!("labels.\"nerdctl/name\"=={}", quote(name_or_id))],
                },
                &self.ns,
            ))
            .await?
            .into_inner()
            .containers;

        let container = match by_name.into_iter().next() {
            Some(c) => c,
            None => {
                // Fall back to treating the argument as a (prefix of a) container id.
                let all = containers
                    .list(with_ns(ListContainersRequest { filters: vec![] }, &self.ns))
                    .await?
                    .into_inner()
                    .containers;
                let mut hits = all.into_iter().filter(|c| c.id.starts_with(name_or_id));
                match (hits.next(), hits.next()) {
                    (Some(c), None) => c,
                    (None, _) => return Err("container not found".into()),
                    (Some(_), Some(_)) => return Err("ambiguous container id prefix".into()),
                }
            }
        };

        if let Some(owner) = owner_pubkey {
            if container.labels.get("owner_pubkey").map(String::as_str) != Some(owner) {
                // Same message as "not found": don't reveal other tenants' names.
                return Err("container not found".into());
            }
        }

        // Full record, for the OCI spec the exec process is derived from.
        let full = containers
            .get(with_ns(
                GetContainerRequest {
                    id: container.id.clone(),
                },
                &self.ns,
            ))
            .await?
            .into_inner()
            .container
            .ok_or("container record missing")?;
        let spec = full
            .spec
            .as_ref()
            .map(|any| serde_json::from_slice::<serde_json::Value>(&any.value))
            .transpose()?
            .unwrap_or(serde_json::Value::Null);
        Ok((full.id, spec))
    }

    /// Block until the container's main process exits and return its exit
    /// code — `Task.Wait` on the init process, so no polling. A container
    /// that already stopped returns at once; one with no task (never
    /// started, or already reaped by the runtime) is reported as `None`.
    pub async fn wait_container(
        &self,
        name_or_id: &str,
        owner_pubkey: Option<&str>,
        timeout: Duration,
    ) -> Result<Option<i32>, ExecError> {
        let (container_id, _) = self.resolve(name_or_id, owner_pubkey).await?;
        let mut tasks = TasksClient::new(self.channel().await?);
        let got = tasks
            .get(with_ns(
                GetRequest {
                    container_id: container_id.clone(),
                    exec_id: String::new(),
                },
                &self.ns,
            ))
            .await;
        let process = match got {
            Ok(r) => r.into_inner().process,
            Err(e) if e.code() == containerd_client::tonic::Code::NotFound => return Ok(None),
            Err(e) => return Err(format!("containerd get: {}", e.message()).into()),
        };
        if let Some(p) = &process {
            // containerd.v1.types.Status::Stopped == 3
            if p.status == 3 {
                return Ok(Some(p.exit_status as i32));
            }
        }
        let waited = tokio::time::timeout(
            timeout,
            tasks.wait(with_ns(
                WaitRequest {
                    container_id,
                    exec_id: String::new(),
                },
                &self.ns,
            )),
        )
        .await;
        match waited {
            Ok(Ok(r)) => Ok(Some(r.into_inner().exit_status as i32)),
            Ok(Err(e)) if e.code() == containerd_client::tonic::Code::NotFound => Ok(None),
            Ok(Err(e)) => Err(format!("containerd wait: {}", e.message()).into()),
            Err(_) => Err("timeout".into()),
        }
    }

    /// Start `spec.cmd` inside the container. The returned process is already
    /// running when this resolves.
    pub async fn spawn(&self, spec: ExecSpec) -> Result<ExecProcess, ExecError> {
        if spec.cmd.is_empty() {
            return Err("empty command".into());
        }
        let (container_id, oci) = self
            .resolve(&spec.container, spec.owner_pubkey.as_deref())
            .await?;
        let exec_id = format!("nk-{}", uuid::Uuid::new_v4().simple());
        let fifo_dir = Path::new(FIFO_ROOT).join(&exec_id);
        std::fs::create_dir_all(&fifo_dir)?;
        std::fs::set_permissions(
            &fifo_dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )?;

        // A TTY always has stdin (it is the terminal). Without one, the caller
        // decides; see `ExecSpec::stdin`.
        let with_stdin = spec.tty || spec.stdin;
        let stdin_p = fifo_dir.join("stdin");
        let stdout_p = fifo_dir.join("stdout");
        let stderr_p = fifo_dir.join("stderr");
        if with_stdin {
            mkfifo(&stdin_p, Mode::S_IRUSR | Mode::S_IWUSR)?;
        }
        mkfifo(&stdout_p, Mode::S_IRUSR | Mode::S_IWUSR)?;
        if !spec.tty {
            mkfifo(&stderr_p, Mode::S_IRUSR | Mode::S_IWUSR)?;
        }

        // Opening a FIFO blocks until the other end is opened, and the shim
        // opens its ends only once the process starts. So open ours on
        // blocking threads now, issue Exec + Start, then collect the opens —
        // the Go client does the same with containerd/fifo, using polling.
        let open_stdin = if with_stdin {
            let p = stdin_p.clone();
            Some(tokio::task::spawn_blocking(move || {
                std::fs::OpenOptions::new().write(true).open(p)
            }))
        } else {
            None
        };
        let open_stdout = {
            let p = stdout_p.clone();
            tokio::task::spawn_blocking(move || std::fs::File::open(p))
        };
        let open_stderr = if spec.tty {
            None
        } else {
            let p = stderr_p.clone();
            Some(tokio::task::spawn_blocking(move || std::fs::File::open(p)))
        };

        let process_spec = build_process_spec(&oci, &spec);
        let mut tasks = TasksClient::new(self.channel().await?);
        let exec_req = ExecProcessRequest {
            container_id: container_id.clone(),
            exec_id: exec_id.clone(),
            terminal: spec.tty,
            stdin: if with_stdin {
                stdin_p.to_string_lossy().into_owned()
            } else {
                String::new()
            },
            stdout: stdout_p.to_string_lossy().into_owned(),
            stderr: if spec.tty {
                String::new()
            } else {
                stderr_p.to_string_lossy().into_owned()
            },
            spec: Some(prost_types::Any {
                type_url: "types.containerd.io/opencontainers/runtime-spec/1/Process".into(),
                value: serde_json::to_vec(&process_spec)?,
            }),
        };

        let inner = Arc::new(ProcInner {
            tasks: tasks.clone(),
            ns: self.ns.clone(),
            container_id: container_id.clone(),
            exec_id: exec_id.clone(),
            fifo_dir: fifo_dir.clone(),
            tty: spec.tty,
            exit: Mutex::new(None),
        });

        let created = tasks.exec(with_ns(exec_req, &self.ns)).await;
        if let Err(e) = created {
            unblock_fifos(&fifo_dir, spec.tty);
            let _ = std::fs::remove_dir_all(&fifo_dir);
            return Err(format!("containerd exec: {}", e.message()).into());
        }

        // The shim opens its FIFO ends while handling Start (runc shim:
        // execProcess.start → openStdin / pio.Copy / CopyConsole), so Start
        // must be issued before we wait on our own opens — otherwise both
        // sides wait for each other.
        let started = tasks
            .start(with_ns(
                StartRequest {
                    container_id: container_id.clone(),
                    exec_id: exec_id.clone(),
                },
                &self.ns,
            ))
            .await;
        if let Err(e) = started {
            // The opener threads are blocked in open(2) on FIFOs the shim never
            // touched; unlinking the FIFOs does not wake them. Open the far ends
            // first or every failed start leaks three blocking-pool threads, and
            // ~170 failures later no exec on this node can attach stdio at all.
            unblock_fifos(&fifo_dir, spec.tty);
            inner.cleanup().await;
            return Err(format!("containerd start: {}", e.message()).into());
        }
        // All opens must complete now that the shim holds its ends.
        let opened = tokio::time::timeout(FIFO_OPEN_TIMEOUT, async {
            let stdin = match open_stdin {
                Some(h) => Some(h.await??),
                None => None,
            };
            let stdout = open_stdout.await??;
            let stderr = match open_stderr {
                Some(h) => Some(h.await??),
                None => None,
            };
            Ok::<_, ExecError>((stdin, stdout, stderr))
        })
        .await;
        let (stdin_f, stdout_f, stderr_f) = match opened {
            Ok(Ok(files)) => files,
            Ok(Err(e)) => {
                unblock_fifos(&fifo_dir, spec.tty);
                inner.cleanup().await;
                return Err(format!("exec: stdio attach failed: {e}").into());
            }
            Err(_) => {
                unblock_fifos(&fifo_dir, spec.tty);
                inner.cleanup().await;
                return Err("exec: shim did not attach stdio within 15s".into());
            }
        };

        debug!("exec {exec_id} started in {container_id}");

        if let (true, Some(size)) = (spec.tty, spec.initial_size) {
            let _ = inner.resize(size).await;
        }

        // ---- IO pumps -------------------------------------------------------
        let (out_tx, out_rx) = mpsc::channel::<(u8, Vec<u8>)>(64);
        let in_tx = stdin_f.map(|stdin_f| {
            let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(64);
            let mut stdin_f = tokio::fs::File::from_std(stdin_f);
            let stdin_inner = inner.clone();
            tokio::spawn(async move {
                while let Some(chunk) = in_rx.recv().await {
                    if stdin_f.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
                drop(stdin_f);
                // Tell the shim the client is done writing (EOF in the container).
                let _ = stdin_inner.close_stdin().await;
            });
            in_tx
        });

        spawn_reader(tokio::fs::File::from_std(stdout_f), STDOUT, out_tx.clone());
        if let Some(f) = stderr_f {
            spawn_reader(tokio::fs::File::from_std(f), STDERR, out_tx.clone());
        }
        drop(out_tx); // readers hold the only senders; `output` ends when they do

        self.touch(&container_id);
        Ok(ExecProcess {
            exec_id,
            container_id,
            stdin: in_tx,
            output: out_rx,
            inner,
        })
    }
}

impl ExecProcess {
    pub async fn resize(&self, size: Resize) -> Result<(), ExecError> {
        self.inner.resize(size).await
    }

    pub async fn kill(&self, signal: i32) -> Result<(), ExecError> {
        self.inner.kill(signal).await
    }

    /// Block until the process exits and return its exit code. Safe to call
    /// once; the FIFO directory and containerd process record are removed
    /// after the readers have had a moment to drain.
    pub async fn wait(&mut self) -> Result<i32, ExecError> {
        if let Some(code) = *self.inner.exit.lock().await {
            return Ok(code);
        }
        let mut tasks = self.inner.tasks.clone();
        let resp = tasks
            .wait(with_ns(
                WaitRequest {
                    container_id: self.inner.container_id.clone(),
                    exec_id: self.inner.exec_id.clone(),
                },
                &self.inner.ns,
            ))
            .await?
            .into_inner();
        let code = resp.exit_status as i32;
        *self.inner.exit.lock().await = Some(code);
        Ok(code)
    }

    /// Kill (if still running), wait for the shim to report the death, and
    /// remove every trace. Idempotent.
    pub async fn abort(&mut self) {
        if self.inner.exit.lock().await.is_none() {
            let _ = self.inner.kill(9).await;
            let _ = tokio::time::timeout(KILL_WAIT, self.wait()).await;
        }
        self.inner.cleanup().await;
    }

    /// Normal end-of-session cleanup after `wait` returned. The readers hit
    /// EOF before `wait` is called, so there is nothing left to drain.
    pub async fn finish(&self) {
        self.inner.cleanup().await;
    }
}

impl ProcInner {
    async fn resize(&self, size: Resize) -> Result<(), ExecError> {
        if !self.tty {
            return Ok(());
        }
        let mut tasks = self.tasks.clone();
        tasks
            .resize_pty(with_ns(
                ResizePtyRequest {
                    container_id: self.container_id.clone(),
                    exec_id: self.exec_id.clone(),
                    width: size.width as u32,
                    height: size.height as u32,
                },
                &self.ns,
            ))
            .await?;
        Ok(())
    }

    async fn kill(&self, signal: i32) -> Result<(), ExecError> {
        let mut tasks = self.tasks.clone();
        tasks
            .kill(with_ns(
                KillRequest {
                    container_id: self.container_id.clone(),
                    exec_id: self.exec_id.clone(),
                    signal: signal as u32,
                    all: false,
                },
                &self.ns,
            ))
            .await?;
        Ok(())
    }

    async fn close_stdin(&self) -> Result<(), ExecError> {
        let mut tasks = self.tasks.clone();
        tasks
            .close_io(with_ns(
                CloseIoRequest {
                    container_id: self.container_id.clone(),
                    exec_id: self.exec_id.clone(),
                    stdin: true,
                },
                &self.ns,
            ))
            .await?;
        Ok(())
    }

    async fn cleanup(&self) {
        let mut tasks = self.tasks.clone();
        let r = tasks
            .delete_process(with_ns(
                DeleteProcessRequest {
                    container_id: self.container_id.clone(),
                    exec_id: self.exec_id.clone(),
                },
                &self.ns,
            ))
            .await;
        if let Err(e) = r {
            // NotFound is normal when the process was already reaped.
            if e.code() != containerd_client::tonic::Code::NotFound {
                warn!("exec {} delete_process: {}", self.exec_id, e.message());
            }
        }
        let _ = std::fs::remove_dir_all(&self.fifo_dir);
    }
}

fn spawn_reader(mut file: tokio::fs::File, channel: u8, tx: mpsc::Sender<(u8, Vec<u8>)>) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; OUTPUT_CHUNK];
        loop {
            match file.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send((channel, buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// Derive the exec process from the container's own OCI process so user,
/// capabilities, rlimits and the image's PATH carry over — then apply the
/// request. This mirrors what `nerdctl exec` does.
fn build_process_spec(oci: &serde_json::Value, spec: &ExecSpec) -> serde_json::Value {
    let mut p = oci
        .get("process")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    p["terminal"] = serde_json::Value::Bool(spec.tty);
    p["args"] = serde_json::json!(if spec.tty || spec.stdin {
        spec.cmd.clone()
    } else {
        without_stdin(&spec.cmd)
    });
    if let Some(cwd) = &spec.cwd {
        p["cwd"] = serde_json::Value::String(cwd.clone());
    } else if p
        .get("cwd")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .is_empty()
    {
        p["cwd"] = serde_json::Value::String("/".into());
    }
    let mut env: Vec<String> = p
        .get("env")
        .and_then(|e| serde_json::from_value(e.clone()).ok())
        .unwrap_or_default();
    if spec.tty && !env.iter().any(|e| e.starts_with("TERM=")) {
        env.push("TERM=xterm-256color".into());
    }
    for kv in &spec.env {
        let key = kv.split('=').next().unwrap_or("");
        env.retain(|e| e.split('=').next() != Some(key));
        env.push(kv.clone());
    }
    p["env"] = serde_json::json!(env);
    if let Some(user) = &spec.user {
        let mut parts = user.splitn(2, ':');
        let uid = parts.next().and_then(|s| s.parse::<u32>().ok());
        let gid = parts.next().and_then(|s| s.parse::<u32>().ok());
        if let Some(uid) = uid {
            p["user"] = serde_json::json!({"uid": uid, "gid": gid.unwrap_or(0)});
        }
    }
    p
}

/// Run `cmd` with stdin redirected from `/dev/null`.
///
/// containerd has no "no stdin" for an exec process: with no FIFO the runc
/// shim gives the process `/dev/null`, but the Kata agent keeps a pipe open
/// that only `CloseIO` would ever close — and `cat` then waits forever. The
/// redirect makes both runtimes behave like `kubectl exec` without `-i`.
/// `exec` keeps the pid, so signals and `Kill` still reach the real process.
fn without_stdin(cmd: &[String]) -> Vec<String> {
    let mut wrapped = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "exec \"$0\" \"$@\" </dev/null".to_string(),
    ];
    wrapped.extend(cmd.iter().cloned());
    wrapped
}

/// Open-and-close the far ends of our FIFOs so blocked opener threads return.
/// Missing FIFOs (no stdin attached) are simply skipped.
fn unblock_fifos(dir: &Path, tty: bool) {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(dir.join("stdin"));
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(dir.join("stdout"));
    if !tty {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(dir.join("stderr"));
    }
}

fn with_ns<T>(msg: T, ns: &str) -> Request<T> {
    let mut req = Request::new(msg);
    req.metadata_mut().insert(
        "containerd-namespace",
        ns.parse().expect("namespace is ascii"),
    );
    req
}

/// containerd filter syntax quotes values with double quotes.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(tty: bool) -> ExecSpec {
        ExecSpec {
            container: "web".into(),
            owner_pubkey: None,
            cmd: vec!["sh".into(), "-c".into(), "id".into()],
            env: vec!["FOO=bar".into(), "PATH=/custom".into()],
            cwd: None,
            user: Some("1000:1000".into()),
            tty,
            initial_size: None,
            stdin: true,
        }
    }

    #[test]
    fn no_stdin_redirects_from_dev_null() {
        let mut s = spec(false);
        s.stdin = false;
        let p = build_process_spec(&serde_json::Value::Null, &s);
        assert_eq!(
            p["args"],
            serde_json::json!([
                "/bin/sh",
                "-c",
                "exec \"$0\" \"$@\" </dev/null",
                "sh",
                "-c",
                "id"
            ])
        );
        // A TTY is its own stdin: never wrapped.
        let mut t = spec(true);
        t.stdin = false;
        let p = build_process_spec(&serde_json::Value::Null, &t);
        assert_eq!(p["args"], serde_json::json!(["sh", "-c", "id"]));
    }

    #[test]
    fn process_spec_inherits_and_overrides() {
        let oci = serde_json::json!({"process": {
            "args": ["nginx"], "cwd": "/app",
            "env": ["PATH=/usr/bin", "HOME=/root"],
            "user": {"uid": 0, "gid": 0},
            "capabilities": {"bounding": ["CAP_CHOWN"]}
        }});
        let p = build_process_spec(&oci, &spec(true));
        assert_eq!(p["args"], serde_json::json!(["sh", "-c", "id"]));
        assert_eq!(p["cwd"], "/app");
        assert_eq!(p["terminal"], true);
        assert_eq!(p["user"]["uid"], 1000);
        assert!(p["capabilities"]["bounding"].is_array());
        let env: Vec<String> = serde_json::from_value(p["env"].clone()).unwrap();
        assert!(env.contains(&"HOME=/root".to_string()));
        assert!(env.contains(&"PATH=/custom".to_string()));
        assert!(!env.contains(&"PATH=/usr/bin".to_string()));
        assert!(env.iter().any(|e| e.starts_with("TERM=")));
    }

    #[test]
    fn process_spec_without_base() {
        let p = build_process_spec(&serde_json::Value::Null, &spec(false));
        assert_eq!(p["cwd"], "/");
        assert_eq!(p["terminal"], false);
        assert!(!p["env"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap().starts_with("TERM=")));
    }

    #[test]
    fn filter_quoting() {
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
    }
}

/// Live tests against a local containerd. They need a running container
/// labelled nerdctl/name=web and owner_pubkey=TESTKEY, so they are ignored
/// by default:  cargo test --bin container-api -- --ignored live_
#[cfg(test)]
mod live {
    use super::*;
    use crate::stream::protocol::{STDERR, STDOUT};

    fn backend() -> ContainerdExec {
        ContainerdExec::new("/run/containerd/containerd.sock", "default")
    }

    fn base(cmd: &[&str], tty: bool) -> ExecSpec {
        ExecSpec {
            container: "web".into(),
            owner_pubkey: Some("TESTKEY".into()),
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            env: vec![],
            cwd: None,
            user: None,
            tty,
            initial_size: None,
            stdin: true,
        }
    }

    #[tokio::test]
    #[ignore]
    async fn live_no_stdin_gets_eof_and_keeps_all_output() {
        let mut spec = base(
            &[
                "/bin/sh",
                "-c",
                "cat; for i in 1 2 3 4 5; do echo line $i; sleep 0.05; done",
            ],
            false,
        );
        spec.stdin = false;
        let mut p = backend().spawn(spec).await.expect("spawn");
        assert!(p.stdin.is_none());
        let mut out = Vec::new();
        while let Some((ch, b)) = p.output.recv().await {
            if ch == STDOUT {
                out.extend(b);
            }
        }
        assert_eq!(p.wait().await.unwrap(), 0);
        p.finish().await;
        assert_eq!(
            String::from_utf8_lossy(&out),
            "line 1\nline 2\nline 3\nline 4\nline 5\n"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn live_exec_collects_streams_and_exit_code() {
        let mut p = backend()
            .spawn(base(
                &["/bin/sh", "-c", "echo out; echo err >&2; exit 7"],
                false,
            ))
            .await
            .expect("spawn");
        drop(p.stdin.take());
        let mut out = Vec::new();
        let mut err = Vec::new();
        while let Some((ch, b)) = p.output.recv().await {
            match ch {
                STDOUT => out.extend(b),
                STDERR => err.extend(b),
                _ => {}
            }
        }
        assert_eq!(p.wait().await.unwrap(), 7);
        p.finish().await;
        assert_eq!(String::from_utf8_lossy(&out), "out\n");
        assert_eq!(String::from_utf8_lossy(&err), "err\n");
    }

    #[tokio::test]
    #[ignore]
    async fn live_stdin_eof_terminates_cat() {
        let mut p = backend()
            .spawn(base(&["/bin/cat"], false))
            .await
            .expect("spawn");
        let stdin = p.stdin.take().unwrap();
        stdin.send(b"ping\n".to_vec()).await.unwrap();
        drop(stdin); // EOF
        let mut out = Vec::new();
        while let Some((_, b)) = p.output.recv().await {
            out.extend(b);
        }
        assert_eq!(p.wait().await.unwrap(), 0);
        p.finish().await;
        assert_eq!(String::from_utf8_lossy(&out), "ping\n");
    }

    #[tokio::test]
    #[ignore]
    async fn live_tty_resize_is_seen_by_shell() {
        let mut spec = base(&["/bin/sh"], true);
        spec.initial_size = Some(Resize {
            width: 80,
            height: 24,
        });
        let mut p = backend().spawn(spec).await.expect("spawn");
        let stdin = p.stdin.take().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        p.resize(Resize {
            width: 120,
            height: 40,
        })
        .await
        .unwrap();
        stdin.send(b"stty size; exit 3\n".to_vec()).await.unwrap();
        let mut out = Vec::new();
        while let Some((_, b)) = p.output.recv().await {
            out.extend(b);
        }
        let code = p.wait().await.unwrap();
        p.finish().await;
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("40 120"), "tty output: {text:?}");
        assert_eq!(code, 3);
    }

    #[tokio::test]
    #[ignore]
    async fn live_wrong_owner_is_not_found() {
        let mut spec = base(&["/bin/true"], false);
        spec.owner_pubkey = Some("SOMEONE-ELSE".into());
        let err = backend().spawn(spec).await.err().expect("must fail");
        assert_eq!(err.to_string(), "container not found");
    }
}

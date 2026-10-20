// init.rs — the PID 1 duties: run the image's own entrypoint as a child,
// reap orphans, and turn SIGTERM into an orderly shutdown.
//
// Reaping has one subtlety: tokio's `Child` waits for its own pids, and a
// blanket `waitpid(-1)` here would steal those exit statuses from the exec
// handlers. So the reaper peeks with WNOWAIT first and only reaps pids that
// nobody else is tracking (orphans re-parented to us).

use nix::sys::signal::{kill, Signal};
#[cfg(target_os = "linux")]
use nix::sys::wait::{waitid, Id, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
#[cfg(target_os = "linux")]
use tracing::debug;
use tracing::{info, warn};

/// Pids whose exit status belongs to a tokio `Child` somewhere in this process.
#[derive(Clone, Default)]
pub struct Tracked(Arc<Mutex<HashSet<i32>>>);

impl Tracked {
    pub fn add(&self, pid: i32) {
        self.0.lock().unwrap().insert(pid);
    }
    pub fn remove(&self, pid: i32) {
        self.0.lock().unwrap().remove(&pid);
    }
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn contains(&self, pid: i32) -> bool {
        self.0.lock().unwrap().contains(&pid)
    }
}

/// Reap orphaned zombies on every SIGCHLD. Runs only when we are PID 1 (or
/// a subreaper); elsewhere the real init does this.
#[cfg(not(target_os = "linux"))]
pub async fn reaper(_tracked: Tracked) {
    // Containers are Linux; on other hosts this binary only builds for tests.
}

#[cfg(target_os = "linux")]
pub async fn reaper(tracked: Tracked) {
    let mut sigchld = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()) {
        Ok(s) => s,
        Err(e) => {
            warn!("cannot listen for SIGCHLD: {e}");
            return;
        }
    };
    loop {
        sigchld.recv().await;
        loop {
            match waitid(
                Id::All,
                WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
            ) {
                Ok(WaitStatus::StillAlive) => break,
                Ok(status) => {
                    let Some(pid) = status.pid() else { break };
                    if tracked.contains(pid.as_raw()) {
                        // tokio owns this one; it will reap it on the same SIGCHLD.
                        break;
                    }
                    match nix::sys::wait::waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                        Ok(s) => debug!("reaped orphan {pid}: {s:?}"),
                        Err(e) => {
                            debug!("waitpid {pid}: {e}");
                            break;
                        }
                    }
                }
                Err(nix::errno::Errno::ECHILD) => break,
                Err(e) => {
                    debug!("waitid: {e}");
                    break;
                }
            }
        }
    }
}

/// Start the sandbox's main program (the image's entrypoint + cmd, or what
/// the deploy asked for) and return a watch that yields its exit code.
pub fn spawn_main(
    argv: &[String],
    tracked: &Tracked,
) -> Result<(i32, watch::Receiver<Option<i32>>), std::io::Error> {
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.kill_on_drop(false);
    let mut child = cmd.spawn()?;
    let pid = child.id().map(|p| p as i32).unwrap_or(0);
    tracked.add(pid);
    info!("main process started: pid {pid}, {}", argv.join(" "));
    let (tx, rx) = watch::channel(None);
    let tracked = tracked.clone();
    tokio::spawn(async move {
        let code = match child.wait().await {
            Ok(status) => status
                .code()
                .unwrap_or_else(|| 128 + signal_of(&status).unwrap_or(0)),
            Err(e) => {
                warn!("wait on main process: {e}");
                1
            }
        };
        tracked.remove(pid);
        info!("main process exited with {code}");
        let _ = tx.send(Some(code));
    });
    Ok((pid, rx))
}

fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

/// On SIGTERM/SIGINT: pass SIGTERM to the main process, give it `grace`, then
/// SIGKILL. Returns once the signal arrived (the caller watches the exit).
pub async fn forward_termination(main_pid: Option<i32>, grace: Duration) {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {},
        _ = int.recv() => {},
    }
    if let Some(pid) = main_pid {
        info!("termination requested: signalling main process {pid}");
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        tokio::time::sleep(grace).await;
        let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    }
}

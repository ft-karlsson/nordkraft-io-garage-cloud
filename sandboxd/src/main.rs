// nordkraft-sandboxd — PID 1 of a Nordkraft sandbox container.
//
// The node bind-mounts this binary read-only into the container and makes it
// the entrypoint, passing the image's own entrypoint/cmd (or the deploy's
// `command`) as arguments. It runs that as a child, reaps orphans, and serves
// exec, framed process streams, PTYs and file transfer over HTTP on the
// container's own address — so nothing a sandbox client does has to go
// through the runtime's exec path.
//
// Environment:
//   NORDKRAFT_SANDBOX_TOKEN     bearer token clients must present (required)
//   NORDKRAFT_SANDBOX_PORT      listen port, default 7777
//   NORDKRAFT_SANDBOX_BIND      listen address, default 0.0.0.0
//   NORDKRAFT_SANDBOX_INSECURE  "1" allows running without a token (dev)
//   RUST_LOG                    tracing filter, default "info"

mod exec;
mod files;
mod init;
mod protocol;
mod server;

use std::time::{Duration, Instant};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let token = std::env::var("NORDKRAFT_SANDBOX_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    if token.is_none() && std::env::var("NORDKRAFT_SANDBOX_INSECURE").ok().as_deref() != Some("1") {
        error!("NORDKRAFT_SANDBOX_TOKEN is not set; refusing to serve an unauthenticated sandbox API (set NORDKRAFT_SANDBOX_INSECURE=1 for development)");
        std::process::exit(2);
    }
    let port: u16 = std::env::var("NORDKRAFT_SANDBOX_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7777);
    let bind = std::env::var("NORDKRAFT_SANDBOX_BIND").unwrap_or_else(|_| "0.0.0.0".into());

    let tracked = init::Tracked::default();
    let is_init = std::process::id() == 1;
    if is_init {
        tokio::spawn(init::reaper(tracked.clone()));
    }

    // The sandbox's own program. Without one we just keep the container alive.
    let (main_pid, mut main_exit) = if argv.is_empty() {
        info!("no main command given; idling as the container's main process");
        (None, None)
    } else {
        match init::spawn_main(&argv, &tracked) {
            Ok((pid, rx)) => (Some(pid), Some(rx)),
            Err(e) => {
                error!("cannot start main process {:?}: {e}", argv);
                std::process::exit(126);
            }
        }
    };

    let state = server::AppState {
        token: token.map(std::sync::Arc::new),
        tracked: tracked.clone(),
        started: Instant::now(),
        main_pid,
    };
    let listener = match tokio::net::TcpListener::bind((bind.as_str(), port)).await {
        Ok(l) => l,
        Err(e) => {
            error!("bind {bind}:{port}: {e}");
            std::process::exit(2);
        }
    };
    info!(
        "nordkraft-sandboxd {} listening on {bind}:{port} (pid {}, init={is_init})",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );
    let server = axum::serve(listener, server::router(state));

    let exit_code = tokio::select! {
        r = server => {
            if let Err(e) = r { error!("server: {e}"); }
            1
        }
        _ = init::forward_termination(main_pid, Duration::from_secs(10)) => {
            // Main process was signalled; take its code if it reported one.
            match main_exit.as_mut() {
                Some(rx) => {
                    let _ = tokio::time::timeout(Duration::from_secs(12), rx.changed()).await;
                    rx.borrow().unwrap_or(143)
                }
                None => 0,
            }
        }
        code = async {
            match main_exit.as_mut() {
                Some(rx) => {
                    loop {
                        if let Some(c) = *rx.borrow() { break c; }
                        if rx.changed().await.is_err() { break 1; }
                    }
                }
                None => std::future::pending().await,
            }
        } => {
            warn!("main process exited ({code}); the sandbox stops with it");
            code
        }
    };
    std::process::exit(exit_code);
}

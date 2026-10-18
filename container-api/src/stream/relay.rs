// stream/relay.rs — WebSocket ↔ channel pumps, and the controller→agent leg.
//
// `pump_ws` is the only place that touches a WebSocket. It is generic over
// the stream type so the same function serves rocket_ws's server-side
// DuplexStream and tokio-tungstenite's client-side stream. The relay on the
// controller is then just two pumps with their channels crossed — it never
// looks inside a frame, which is the property that keeps it correct.

use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, Message};
use tracing::debug;

use super::containerd_exec::ExecError;

const PING_EVERY: Duration = Duration::from_secs(30);
/// Two missed pongs and the peer is considered dead.
const PONG_GRACE: Duration = Duration::from_secs(75);

/// Move frames between a WebSocket and a pair of channels until either side
/// finishes. Text frames are accepted as binary (browser clients); pings are
/// answered by tungstenite; we send our own pings and give up on silence.
pub async fn pump_ws<S>(
    mut ws: S,
    to_app: mpsc::Sender<Vec<u8>>,
    mut from_app: mpsc::Receiver<Vec<u8>>,
) where
    S: StreamExt<Item = Result<Message, tungstenite::Error>>
        + SinkExt<Message, Error = tungstenite::Error>
        + Unpin,
{
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_heard = tokio::time::Instant::now();
    loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    last_heard = tokio::time::Instant::now();
                    if to_app.send(b).await.is_err() { break; }
                }
                Some(Ok(Message::Text(t))) => {
                    last_heard = tokio::time::Instant::now();
                    if to_app.send(t.into_bytes()).await.is_err() { break; }
                }
                Some(Ok(Message::Pong(_))) | Some(Ok(Message::Ping(_))) => {
                    last_heard = tokio::time::Instant::now();
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(Message::Frame(_))) => {}
                Some(Err(e)) => { debug!("ws read: {e}"); break; }
            },
            out = from_app.recv() => match out {
                Some(b) => {
                    if ws.send(Message::Binary(b)).await.is_err() { break; }
                }
                None => {
                    // Application side finished: say goodbye properly.
                    let _ = ws.send(Message::Close(None)).await;
                    break;
                }
            },
            _ = ping.tick() => {
                if last_heard.elapsed() > PONG_GRACE { debug!("ws peer silent, closing"); break; }
                if ws.send(Message::Ping(Vec::new())).await.is_err() { break; }
            }
        }
    }
    let _ = ws.close().await;
}

/// Controller leg: open the agent's internal stream and return it as a
/// WebSocket the caller can pump like any other.
pub async fn connect_agent(
    url: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    ExecError,
> {
    let (ws, _resp) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(url),
    )
    .await
    .map_err(|_| "agent did not answer within 10s")??;
    Ok(ws)
}

/// Build the agent-side URL for a container stream. `query` is passed through
/// verbatim — the controller does not reinterpret the client's request.
pub fn agent_stream_url(agent_ip: &str, agent_port: u16, container: &str, query: &str) -> String {
    format!(
        "ws://{}:{agent_port}/api/internal/containers/{container}/stream?{query}",
        host_for_url(agent_ip)
    )
}

/// Bracket IPv6 literals for use in a URL.
pub fn host_for_url(ip: &str) -> String {
    if ip.contains(':') && !ip.starts_with('[') {
        format!("[{ip}]")
    } else {
        ip.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_shape() {
        assert_eq!(
            agent_stream_url("172.20.0.11", 8001, "web", "tty=1&cmd=sh"),
            "ws://172.20.0.11:8001/api/internal/containers/web/stream?tty=1&cmd=sh"
        );
        assert!(agent_stream_url("fd00::1", 8001, "web", "").starts_with("ws://[fd00::1]:8001/"));
    }
}

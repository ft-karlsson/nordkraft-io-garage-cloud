// stream/protocol.rs — wire framing for exec / shell sessions.
//
// This is the Kubernetes "v4.channel.k8s.io" protocol, used verbatim so the
// shape is already known to anyone who has read kubectl: every WebSocket
// frame is binary, and the first byte says which channel the rest belongs to.
//
//   0  stdin   client → container
//   1  stdout  container → client
//   2  stderr  container → client   (unused in TTY mode: a pty merges them)
//   3  status  container → client   JSON, sent exactly once before close
//   4  resize  client → container   JSON {"width":cols,"height":rows}
//
// The controller relays frames without parsing them; only the two ends
// (CLI / agent) know what a channel means. Keeping the relay dumb is what
// makes it reliable.

use serde::{Deserialize, Serialize};

pub const STDIN: u8 = 0;
pub const STDOUT: u8 = 1;
pub const STDERR: u8 = 2;
pub const STATUS: u8 = 3;
pub const RESIZE: u8 = 4;

/// Final frame of every session. `exit_code` is None only when the process
/// could not be started or was lost (then `error` says why).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Status {
    pub fn exited(code: i32) -> Self {
        Self {
            exit_code: Some(code),
            error: None,
        }
    }
    pub fn failed(msg: impl Into<String>) -> Self {
        Self {
            exit_code: None,
            error: Some(msg.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Resize {
    pub width: u16,
    pub height: u16,
}

/// Prefix `payload` with its channel byte.
pub fn frame(channel: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(channel);
    out.extend_from_slice(payload);
    out
}

/// Split a received frame into (channel, payload). Empty frames are ignored.
pub fn unframe(data: &[u8]) -> Option<(u8, &[u8])> {
    let (ch, rest) = data.split_first()?;
    Some((*ch, rest))
}

pub fn status_frame(status: &Status) -> Vec<u8> {
    frame(STATUS, &serde_json::to_vec(status).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let f = frame(STDOUT, b"hello");
        assert_eq!(unframe(&f), Some((STDOUT, &b"hello"[..])));
        assert_eq!(unframe(&[]), None);
    }

    #[test]
    fn status_json_is_stable() {
        let s = status_frame(&Status::exited(7));
        assert_eq!(s[0], STATUS);
        assert_eq!(std::str::from_utf8(&s[1..]).unwrap(), r#"{"exit_code":7}"#);
    }
}

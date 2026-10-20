// Wire framing: Kubernetes `v4.channel.k8s.io`, byte-for-byte the same as
// container-api/src/stream/protocol.rs, so the controller relays frames
// from this daemon without looking inside them.
//
//   0 stdin   client → process
//   1 stdout  process → client
//   2 stderr  process → client (unused with a tty)
//   3 status  process → client, JSON, exactly once before close
//   4 resize  client → process, JSON {"width":cols,"height":rows}

use serde::{Deserialize, Serialize};

pub const STDIN: u8 = 0;
pub const STDOUT: u8 = 1;
pub const STDERR: u8 = 2;
pub const STATUS: u8 = 3;
pub const RESIZE: u8 = 4;

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

pub fn frame(channel: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(channel);
    out.extend_from_slice(payload);
    out
}

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
        let f = frame(STDOUT, b"hi");
        assert_eq!(unframe(&f), Some((STDOUT, &b"hi"[..])));
        assert_eq!(
            String::from_utf8(status_frame(&Status::exited(3))[1..].to_vec()).unwrap(),
            r#"{"exit_code":3}"#
        );
    }
}

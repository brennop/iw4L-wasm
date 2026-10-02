//! O16: framing of the WebSocket transport (browser <-> master, plain `ws://`).
//!
//! One WebSocket carries what a QUIC / WebTransport session carries: relay
//! datagrams, one client-opened bidi stream (the control stream) and the
//! master-opened uni streams (bootstrap). Each WebSocket binary message is one
//! frame: a `u8` kind, a little-endian `u32` stream id for the stream kinds,
//! then the payload. Both ends use this module, so the constants live once.
//!
//! | kind | frame | layout after the kind byte |
//! |---|---|---|
//! | 0 | `Datagram` | payload (the same bytes as a WebTransport datagram) |
//! | 1 | `OpenBi` | `u32` id (client ids are even) |
//! | 2 | `OpenUni` | `u32` id (master ids are odd) |
//! | 3 | `Data` | `u32` id, bytes (at most [`MAX_DATA_CHUNK`]) |
//! | 4 | `Fin` | `u32` id |
//! | 5 | `Stop` | `u32` id, `u32` code |
//! | 6 | `Close` | `u32` code, reason bytes |

use core::fmt;

pub const KIND_DATAGRAM: u8 = 0;
pub const KIND_OPEN_BI: u8 = 1;
pub const KIND_OPEN_UNI: u8 = 2;
pub const KIND_DATA: u8 = 3;
pub const KIND_FIN: u8 = 4;
pub const KIND_STOP: u8 = 5;
pub const KIND_CLOSE: u8 = 6;

/// Largest payload of one `Data` frame; larger writes are split.
pub const MAX_DATA_CHUNK: usize = 16 * 1024;

/// Largest whole message the master accepts: a data chunk plus its header
/// (a datagram is at most `MAX_OPAQUE_PAYLOAD` + relay header, far below).
pub const MAX_MESSAGE_BYTES: usize = MAX_DATA_CHUNK + 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsFrame<'a> {
    Datagram(&'a [u8]),
    OpenBi { stream: u32 },
    OpenUni { stream: u32 },
    Data { stream: u32, payload: &'a [u8] },
    Fin { stream: u32 },
    Stop { stream: u32, code: u32 },
    Close { code: u32, reason: &'a [u8] },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsFrameError {
    Empty,
    UnknownKind(u8),
    Truncated,
    TooLong,
}

impl fmt::Display for WsFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty websocket frame"),
            Self::UnknownKind(kind) => write!(f, "unknown websocket frame kind {kind}"),
            Self::Truncated => f.write_str("truncated websocket frame"),
            Self::TooLong => f.write_str("websocket frame too long"),
        }
    }
}

impl core::error::Error for WsFrameError {}

impl WsFrame<'_> {
    /// The frame as one WebSocket message.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Datagram(payload) => {
                out.reserve_exact(1 + payload.len());
                out.push(KIND_DATAGRAM);
                out.extend_from_slice(payload);
            }
            Self::OpenBi { stream } => {
                out.push(KIND_OPEN_BI);
                out.extend_from_slice(&stream.to_le_bytes());
            }
            Self::OpenUni { stream } => {
                out.push(KIND_OPEN_UNI);
                out.extend_from_slice(&stream.to_le_bytes());
            }
            Self::Data { stream, payload } => {
                out.reserve_exact(5 + payload.len());
                out.push(KIND_DATA);
                out.extend_from_slice(&stream.to_le_bytes());
                out.extend_from_slice(payload);
            }
            Self::Fin { stream } => {
                out.push(KIND_FIN);
                out.extend_from_slice(&stream.to_le_bytes());
            }
            Self::Stop { stream, code } => {
                out.push(KIND_STOP);
                out.extend_from_slice(&stream.to_le_bytes());
                out.extend_from_slice(&code.to_le_bytes());
            }
            Self::Close { code, reason } => {
                out.push(KIND_CLOSE);
                out.extend_from_slice(&code.to_le_bytes());
                out.extend_from_slice(reason);
            }
        }
        out
    }
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, WsFrameError> {
    let raw = bytes.get(at..at + 4).ok_or(WsFrameError::Truncated)?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

/// Decodes one WebSocket message. The result borrows the message.
pub fn decode(bytes: &[u8]) -> Result<WsFrame<'_>, WsFrameError> {
    let (&kind, rest) = bytes.split_first().ok_or(WsFrameError::Empty)?;
    Ok(match kind {
        KIND_DATAGRAM => WsFrame::Datagram(rest),
        KIND_OPEN_BI => WsFrame::OpenBi {
            stream: u32_at(rest, 0)?,
        },
        KIND_OPEN_UNI => WsFrame::OpenUni {
            stream: u32_at(rest, 0)?,
        },
        KIND_DATA => {
            let stream = u32_at(rest, 0)?;
            let payload = &rest[4..];
            if payload.len() > MAX_DATA_CHUNK {
                return Err(WsFrameError::TooLong);
            }
            WsFrame::Data { stream, payload }
        }
        KIND_FIN => WsFrame::Fin {
            stream: u32_at(rest, 0)?,
        },
        KIND_STOP => WsFrame::Stop {
            stream: u32_at(rest, 0)?,
            code: u32_at(rest, 4)?,
        },
        KIND_CLOSE => WsFrame::Close {
            code: u32_at(rest, 0)?,
            reason: &rest[4..],
        },
        other => return Err(WsFrameError::UnknownKind(other)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips() {
        let frames = [
            WsFrame::Datagram(b"relay bytes"),
            WsFrame::Datagram(b""),
            WsFrame::OpenBi { stream: 0 },
            WsFrame::OpenUni { stream: 7 },
            WsFrame::Data {
                stream: 0x0102_0304,
                payload: b"hello",
            },
            WsFrame::Data {
                stream: 2,
                payload: b"",
            },
            WsFrame::Fin { stream: 9 },
            WsFrame::Stop {
                stream: 4,
                code: 0xdead_beef,
            },
            WsFrame::Close {
                code: 1,
                reason: b"session closed",
            },
            WsFrame::Close {
                code: 0,
                reason: b"",
            },
        ];
        for frame in frames {
            let bytes = frame.encode();
            assert_eq!(decode(&bytes).unwrap(), frame, "{frame:?}");
        }
    }

    #[test]
    fn kind_bytes_and_little_endian_ids() {
        assert_eq!(WsFrame::Datagram(b"x").encode(), [0, b'x']);
        assert_eq!(WsFrame::OpenBi { stream: 2 }.encode(), [1, 2, 0, 0, 0]);
        assert_eq!(WsFrame::OpenUni { stream: 1 }.encode(), [2, 1, 0, 0, 0]);
        assert_eq!(
            WsFrame::Data {
                stream: 0x0102,
                payload: b"ab"
            }
            .encode(),
            [3, 2, 1, 0, 0, b'a', b'b']
        );
        assert_eq!(WsFrame::Fin { stream: 3 }.encode(), [4, 3, 0, 0, 0]);
        assert_eq!(
            WsFrame::Stop { stream: 3, code: 5 }.encode(),
            [5, 3, 0, 0, 0, 5, 0, 0, 0]
        );
        assert_eq!(
            WsFrame::Close {
                code: 6,
                reason: b"r"
            }
            .encode(),
            [6, 6, 0, 0, 0, b'r']
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(decode(&[]), Err(WsFrameError::Empty));
        assert_eq!(decode(&[9]), Err(WsFrameError::UnknownKind(9)));
        assert_eq!(decode(&[KIND_OPEN_BI, 1, 2]), Err(WsFrameError::Truncated));
        assert_eq!(
            decode(&[KIND_STOP, 1, 0, 0, 0]),
            Err(WsFrameError::Truncated)
        );
        assert_eq!(decode(&[KIND_CLOSE]), Err(WsFrameError::Truncated));
        let mut big = vec![KIND_DATA, 0, 0, 0, 0];
        big.resize(5 + MAX_DATA_CHUNK + 1, 0);
        assert_eq!(decode(&big), Err(WsFrameError::TooLong));
    }
}

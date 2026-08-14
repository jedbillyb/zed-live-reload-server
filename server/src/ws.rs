//! Minimal RFC 6455 server support.
//!
//! Only what the reload channel needs: the opening handshake, unmasked text
//! frames from server to client, and enough of the read path to answer pings
//! and notice when a tab goes away. Pulling in a full WebSocket stack for that
//! would be a lot of dependency for a few dozen lines.

use base64::Engine as _;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Magic value from RFC 6455 section 1.3.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

const OP_TEXT: u8 = 0x1;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// Largest client frame we are willing to buffer. The client never sends us
/// anything of substance, so anything big is a bug or an attack.
const MAX_FRAME: u64 = 64 * 1024;

/// Computes the `Sec-WebSocket-Accept` response value for a client key.
pub fn accept_key(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

/// Serialises a text frame. Server frames are never masked.
pub fn text_frame(payload: &str) -> Vec<u8> {
    let bytes = payload.as_bytes();
    let mut frame = Vec::with_capacity(bytes.len() + 10);
    frame.push(0x80 | OP_TEXT); // FIN + text

    match bytes.len() {
        len if len < 126 => frame.push(len as u8),
        len if len <= u16::MAX as usize => {
            frame.push(126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            frame.push(127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }

    frame.extend_from_slice(bytes);
    frame
}

fn control_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    // Control frames are limited to a 125 byte payload, which conveniently
    // means the length is always a single byte.
    let payload = &payload[..payload.len().min(125)];
    let mut frame = Vec::with_capacity(payload.len() + 2);
    frame.push(0x80 | opcode);
    frame.push(payload.len() as u8);
    frame.extend_from_slice(payload);
    frame
}

/// What a client frame told us to do.
pub enum Incoming {
    /// Send these bytes back verbatim.
    Pong(Vec<u8>),
    /// The peer is closing, or the stream ended.
    Close,
    /// Nothing to act on.
    Ignore,
}

/// Reads one frame from the client.
///
/// Returns [`Incoming::Close`] on any protocol violation rather than trying to
/// resynchronise, since a browser that sends us a malformed frame is not a
/// connection worth keeping.
pub async fn read_frame(reader: &mut OwnedReadHalf) -> Incoming {
    let mut header = [0u8; 2];
    if reader.read_exact(&mut header).await.is_err() {
        return Incoming::Close;
    }

    let opcode = header[0] & 0x0F;
    let masked = header[1] & 0x80 != 0;

    let length = match header[1] & 0x7F {
        126 => {
            let mut extended = [0u8; 2];
            if reader.read_exact(&mut extended).await.is_err() {
                return Incoming::Close;
            }
            u16::from_be_bytes(extended) as u64
        }
        127 => {
            let mut extended = [0u8; 8];
            if reader.read_exact(&mut extended).await.is_err() {
                return Incoming::Close;
            }
            u64::from_be_bytes(extended)
        }
        short => short as u64,
    };

    if length > MAX_FRAME {
        return Incoming::Close;
    }

    let mut mask = [0u8; 4];
    if masked && reader.read_exact(&mut mask).await.is_err() {
        return Incoming::Close;
    }

    let mut payload = vec![0u8; length as usize];
    if !payload.is_empty() && reader.read_exact(&mut payload).await.is_err() {
        return Incoming::Close;
    }

    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }

    match opcode {
        OP_CLOSE => Incoming::Close,
        OP_PING => Incoming::Pong(payload),
        OP_PONG | OP_TEXT => Incoming::Ignore,
        _ => Incoming::Ignore,
    }
}

/// Writes a pong in reply to a ping.
pub async fn send_pong(writer: &mut OwnedWriteHalf, payload: &[u8]) -> std::io::Result<()> {
    writer.write_all(&control_frame(OP_PONG, payload)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_the_handshake_accept_value_from_rfc_6455() {
        // The worked example in RFC 6455 section 1.3.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn encodes_short_payloads_with_a_single_length_byte() {
        let frame = text_frame("hi");
        assert_eq!(frame, vec![0x81, 0x02, b'h', b'i']);
    }

    #[test]
    fn switches_to_the_16_bit_length_form_at_126_bytes() {
        let frame = text_frame(&"x".repeat(126));
        assert_eq!(&frame[..4], &[0x81, 126, 0x00, 126]);
        assert_eq!(frame.len(), 4 + 126);
    }

    #[test]
    fn switches_to_the_64_bit_length_form_past_u16() {
        let payload = "x".repeat(u16::MAX as usize + 1);
        let frame = text_frame(&payload);
        assert_eq!(frame[1], 127);
        assert_eq!(
            u64::from_be_bytes(frame[2..10].try_into().unwrap()),
            payload.len() as u64
        );
    }

    #[test]
    fn caps_control_frame_payloads_at_the_protocol_limit() {
        let frame = control_frame(OP_PONG, &[0u8; 200]);
        assert_eq!(frame[1], 125);
        assert_eq!(frame.len(), 127);
    }
}

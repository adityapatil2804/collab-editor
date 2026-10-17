//! WebSocket framing (RFC 6455) over blocking std streams.
//! Supports text/binary messages, fragmentation, ping/pong and close.

use crate::sha1::{base64, sha1};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};

/// Largest message accepted (bytes). Protects the server from memory blowups.
pub const MAX_MESSAGE: usize = 8 << 20;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;

#[derive(Debug)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

/// `Sec-WebSocket-Accept` value for a client's `Sec-WebSocket-Key`.
pub fn accept_key(key: &str) -> String {
    base64(&sha1(format!("{}{GUID}", key.trim()).as_bytes()))
}

/// Encode one complete frame. Clients must mask; servers must not.
pub fn encode_frame(opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 14);
    f.push(0x80 | opcode);
    let mbit = if mask.is_some() { 0x80 } else { 0 };
    match payload.len() {
        n if n < 126 => f.push(mbit | n as u8),
        n if n <= u16::MAX as usize => {
            f.push(mbit | 126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            f.push(mbit | 127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    match mask {
        Some(m) => {
            f.extend_from_slice(&m);
            f.extend(payload.iter().enumerate().map(|(i, b)| b ^ m[i % 4]));
        }
        None => f.extend_from_slice(payload),
    }
    f
}

pub fn text_frame(s: &str) -> Vec<u8> {
    encode_frame(OP_TEXT, s.as_bytes(), None)
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Read the next complete message, reassembling fragments.
pub fn read_message<R: Read>(r: &mut R) -> io::Result<Message> {
    let mut buf: Vec<u8> = Vec::new();
    let mut kind: Option<u8> = None;
    loop {
        let mut h = [0u8; 2];
        r.read_exact(&mut h)?;
        let fin = h[0] & 0x80 != 0;
        let opcode = h[0] & 0x0F;
        let masked = h[1] & 0x80 != 0;
        let len = match h[1] & 0x7F {
            126 => {
                let mut b = [0u8; 2];
                r.read_exact(&mut b)?;
                u16::from_be_bytes(b) as usize
            }
            127 => {
                let mut b = [0u8; 8];
                r.read_exact(&mut b)?;
                u64::from_be_bytes(b) as usize
            }
            n => n as usize,
        };
        if len > MAX_MESSAGE || buf.len() + len > MAX_MESSAGE {
            return Err(invalid("message too large"));
        }
        let mut key = [0u8; 4];
        if masked {
            r.read_exact(&mut key)?;
        }
        let mut payload = vec![0u8; len];
        r.read_exact(&mut payload)?;
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= key[i % 4];
            }
        }

        match opcode {
            OP_CLOSE => return Ok(Message::Close),
            OP_PING => return Ok(Message::Ping(payload)),
            OP_PONG => return Ok(Message::Pong(payload)),
            0x0 => {
                if kind.is_none() {
                    return Err(invalid("continuation without start"));
                }
                buf.extend_from_slice(&payload);
            }
            OP_TEXT | OP_BINARY => {
                if kind.is_some() {
                    return Err(invalid("new message inside fragmented message"));
                }
                kind = Some(opcode);
                buf = payload;
            }
            _ => return Err(invalid("unknown opcode")),
        }
        if fin {
            return Ok(match kind {
                Some(OP_TEXT) => Message::Text(
                    String::from_utf8(buf).map_err(|_| invalid("text is not UTF-8"))?,
                ),
                _ => Message::Binary(buf),
            });
        }
    }
}

// ------------------------------------------------------------------ client

static MASK_SEED: AtomicU64 = AtomicU64::new(0x9E3779B97F4A7C15);

fn next_mask() -> [u8; 4] {
    let mut x = MASK_SEED.fetch_add(0x9E3779B97F4A7C15, Ordering::Relaxed);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    (x as u32).to_le_bytes()
}

/// A minimal blocking WebSocket client (used by the load tester and tests).
pub struct Client {
    pub reader: BufReader<TcpStream>,
    pub writer: TcpStream,
}

impl Client {
    pub fn connect(addr: &str, path: &str) -> io::Result<Client> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        let mut writer = stream.try_clone()?;
        let key = base64(&next_mask().repeat(4));
        write!(
            writer,
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )?;
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status)?;
        if !status.contains(" 101 ") {
            return Err(invalid(&format!("handshake failed: {}", status.trim())));
        }
        let mut accepted = false;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            if line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                if k.eq_ignore_ascii_case("sec-websocket-accept") && v.trim() == accept_key(&key) {
                    accepted = true;
                }
            }
        }
        if !accepted {
            return Err(invalid("bad Sec-WebSocket-Accept"));
        }
        Ok(Client { reader, writer })
    }

    pub fn send_text(&mut self, s: &str) -> io::Result<()> {
        self.writer
            .write_all(&encode_frame(OP_TEXT, s.as_bytes(), Some(next_mask())))
    }

    pub fn recv(&mut self) -> io::Result<Message> {
        read_message(&mut self.reader)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_accept_example() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn frame_roundtrip_all_length_encodings() {
        for n in [0usize, 5, 125, 126, 70_000] {
            let payload: Vec<u8> = (0..n).map(|i| b'a' + (i % 26) as u8).collect();
            for mask in [None, Some([1, 2, 3, 4])] {
                let f = encode_frame(OP_TEXT, &payload, mask);
                match read_message(&mut &f[..]).unwrap() {
                    Message::Text(s) => assert_eq!(s.as_bytes(), &payload[..]),
                    m => panic!("unexpected {m:?}"),
                }
            }
        }
    }

    #[test]
    fn fragmented_message() {
        let mut f = vec![OP_TEXT, 3];
        f.extend_from_slice(b"hel");
        f.extend_from_slice(&[0x80, 2]);
        f.extend_from_slice(b"lo");
        match read_message(&mut &f[..]).unwrap() {
            Message::Text(s) => assert_eq!(s, "hello"),
            m => panic!("unexpected {m:?}"),
        }
    }
}

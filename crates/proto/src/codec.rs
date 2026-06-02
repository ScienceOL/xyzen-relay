//! Length-delimited framing used by RustDesk on the wire.
//!
//! The on-wire format is a variable-length header (1–4 bytes) whose low 2
//! bits hold `header_len - 1` and whose remaining bits, after a `>> 2`,
//! hold the payload length in little-endian byte order. The decoder is
//! re-implemented from the public protocol description rather than copied
//! from upstream; the format itself is not copyrightable.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io;
use tokio_util::codec::{Decoder, Encoder};

const MAX_PACKET: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
enum State {
    Head,
    Body(usize),
}

#[derive(Debug, Clone, Copy)]
pub struct RustDeskCodec {
    state: State,
    raw: bool,
    max: usize,
}

impl Default for RustDeskCodec {
    fn default() -> Self {
        Self {
            state: State::Head,
            raw: false,
            max: MAX_PACKET,
        }
    }
}

impl RustDeskCodec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Switch into raw passthrough — no framing, hand the buffer up
    /// untouched. The relay flips this on after the handshake.
    pub fn set_raw(&mut self) {
        self.raw = true;
    }
}

impl Decoder for RustDeskCodec {
    type Item = BytesMut;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<BytesMut>> {
        if self.raw {
            if src.is_empty() {
                return Ok(None);
            }
            let n = src.len();
            return Ok(Some(src.split_to(n)));
        }

        loop {
            match self.state {
                State::Head => {
                    if src.is_empty() {
                        return Ok(None);
                    }
                    let head_len = ((src[0] & 0x03) + 1) as usize;
                    if src.len() < head_len {
                        return Ok(None);
                    }
                    let mut n = src[0] as usize;
                    if head_len > 1 {
                        n |= (src[1] as usize) << 8;
                    }
                    if head_len > 2 {
                        n |= (src[2] as usize) << 16;
                    }
                    if head_len > 3 {
                        n |= (src[3] as usize) << 24;
                    }
                    n >>= 2;
                    if n > self.max {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "frame exceeds max packet length",
                        ));
                    }
                    src.advance(head_len);
                    src.reserve(n);
                    self.state = State::Body(n);
                }
                State::Body(n) => {
                    if src.len() < n {
                        return Ok(None);
                    }
                    let frame = src.split_to(n);
                    self.state = State::Head;
                    return Ok(Some(frame));
                }
            }
        }
    }
}

impl Encoder<Bytes> for RustDeskCodec {
    type Error = io::Error;

    fn encode(&mut self, item: Bytes, dst: &mut BytesMut) -> io::Result<()> {
        if self.raw {
            dst.reserve(item.len());
            dst.put_slice(&item);
            return Ok(());
        }

        let n = item.len();
        if n > self.max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame exceeds max packet length",
            ));
        }
        let head_len = if n < (1 << 6) {
            1
        } else if n < (1 << 14) {
            2
        } else if n < (1 << 22) {
            3
        } else {
            4
        };
        let header = ((n as u32) << 2) | (head_len as u32 - 1);
        dst.reserve(head_len + n);
        for i in 0..head_len {
            dst.put_u8(((header >> (i * 8)) & 0xff) as u8);
        }
        dst.put_slice(&item);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_short() {
        let mut codec = RustDeskCodec::new();
        let mut buf = BytesMut::new();
        codec.encode(Bytes::from_static(b"hello"), &mut buf).unwrap();
        let frame = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(&frame[..], b"hello");
    }

    #[test]
    fn roundtrip_two_byte_header() {
        let payload = vec![0xab; 200]; // forces 2-byte header
        let mut codec = RustDeskCodec::new();
        let mut buf = BytesMut::new();
        codec
            .encode(Bytes::from(payload.clone()), &mut buf)
            .unwrap();
        let frame = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(&frame[..], &payload[..]);
    }

    #[test]
    fn roundtrip_three_byte_header() {
        let payload = vec![0xcd; 70_000]; // forces 3-byte header
        let mut codec = RustDeskCodec::new();
        let mut buf = BytesMut::new();
        codec
            .encode(Bytes::from(payload.clone()), &mut buf)
            .unwrap();
        let frame = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame.len(), payload.len());
    }
}

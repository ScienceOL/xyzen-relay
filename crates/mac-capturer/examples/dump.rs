//! `cargo run -p xyzen-relay-mac-capturer --example dump > /tmp/native.h264`
//!
//! Stream raw NAL bytes from the Swift capturer to stdout for ~3s, then exit.
//! Use ffprobe to validate the output afterwards.

use std::io::{self, Write};
use std::time::{Duration, Instant};

fn main() {
    eprintln!("starting native mac capturer (1920x1080 @30fps, 4 Mbps)");
    let rx = xyzen_relay_mac_capturer::start(1920, 1080, 30, 4000).expect("start mac-capturer");
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut bytes = 0u64;
    let mut nals = 0u64;
    while let Ok(nal) = rx.recv_timeout(Duration::from_secs(2)) {
        out.write_all(&nal.data).expect("write");
        bytes += nal.data.len() as u64;
        nals += 1;
        if Instant::now() >= deadline {
            break;
        }
    }
    eprintln!("dumped {nals} NALs, {bytes} bytes");
}

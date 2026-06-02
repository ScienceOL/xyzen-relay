//! RustDesk wire protocol bindings.
//!
//! Vendors `rendezvous.proto` from `rustdesk/hbb_common` (AGPL-3.0) and
//! exposes the generated Prost types alongside a framing codec compatible
//! with the official client (1–4 byte little-endian variable-length header,
//! low 2 bits encode header length minus 1).

pub mod hbb {
    include!(concat!(env!("OUT_DIR"), "/hbb.rs"));
}

pub mod codec;

pub use hbb::*;

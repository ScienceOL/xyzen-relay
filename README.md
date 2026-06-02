# xyzen-relay

A self-hosted RustDesk-compatible rendezvous + relay server, written from
scratch and licensed AGPL-3.0-or-later. Designed to back the remote-desktop
feature for advanced users of [Xyzen](https://xyzen.ai).

## Status

Pre-alpha. Goal of the first milestone is **wire-protocol parity with the
[`rustdesk-server-demo`](https://github.com/rustdesk/rustdesk-server-demo)** —
i.e. an unmodified official RustDesk client can connect to two peers through
this server via TCP relay (no NAT hole-punching yet).

## Layout

| Crate          | Role                                                                      |
| -------------- | ------------------------------------------------------------------------- |
| `proto`        | Generated Prost types from RustDesk's open `rendezvous.proto` + framing   |
| `common`       | Shared utilities (config, logging, address types)                         |
| `rendezvous`   | Replaces `hbbs` — peer registry + punch coordination                      |
| `relay`        | Replaces `hbbr` — bidirectional TCP relay                                 |

## License

This project is **AGPL-3.0-or-later**. RustDesk's wire protocol definitions
(`rendezvous.proto`) are vendored under `crates/proto/protos/` from upstream
`rustdesk/hbb_common`, also AGPL-3.0. Anyone running this server as a
network-accessible service is required by §13 to make the corresponding
source available to its users.

The Xyzen product itself is **not** a derivative of this code: it talks to
`xyzen-relay` only over a network API.

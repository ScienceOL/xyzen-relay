-- Initial schema for the control plane.

CREATE TABLE IF NOT EXISTS peers (
    user_id    TEXT PRIMARY KEY,
    peer_id    TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL  -- unix seconds
);

CREATE INDEX IF NOT EXISTS idx_peers_peer_id ON peers (peer_id);

CREATE TABLE IF NOT EXISTS audit (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    ts                  INTEGER NOT NULL, -- unix seconds
    kind                TEXT NOT NULL,    -- register | punch | disconnect | ...
    peer_id             TEXT,
    controller_peer_id  TEXT,
    addr                TEXT,
    meta_json           TEXT
);

CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit (ts);
CREATE INDEX IF NOT EXISTS idx_audit_peer ON audit (peer_id);

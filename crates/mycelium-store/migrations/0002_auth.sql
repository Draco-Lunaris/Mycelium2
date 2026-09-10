-- Phase 4: auth additions.

-- The service key also seals each user's master key, so a user's bundle can
-- be re-wrapped on password reset (admin-assisted recovery path) without
-- the recovery key. NULL for users created before this feature or when
-- service sealing is disabled.
ALTER TABLE users ADD COLUMN master_key_service_sealed TEXT;

-- WebAuthn credentials (second factor only per DESIGN).
CREATE TABLE webauthn_credentials (
    id TEXT PRIMARY KEY,            -- credential ID (base64url)
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    public_key BLOB NOT NULL,       -- serialized verification key
    counter INTEGER NOT NULL DEFAULT 0,
    transports TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);
CREATE INDEX idx_webauthn_credentials_user ON webauthn_credentials(user_id);

-- WebAuthn challenge state (single-use, expiring).
CREATE TABLE webauthn_challenges (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    challenge BLOB NOT NULL,
    purpose TEXT NOT NULL CHECK (purpose IN ('register', 'login')),
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
CREATE INDEX idx_webauthn_challenges_user ON webauthn_challenges(user_id);
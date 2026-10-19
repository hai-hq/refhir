-- Links, the encrypted files on them, and who has asked. Times are seconds since 1970.
-- `version` is bumped by every change to a link: a change that read a link that has since changed is
-- not kept, and is run again, which is how two processes on one file take turns.
CREATE TABLE links (
  id             TEXT PRIMARY KEY,
  token_hash     TEXT NOT NULL,
  active         INTEGER NOT NULL DEFAULT 1,
  created_at     INTEGER NOT NULL,
  created_by     TEXT NOT NULL,
  expires_at     INTEGER NOT NULL,
  ended_at       INTEGER,
  attempts_left  INTEGER NOT NULL,
  passcode_salt  TEXT,
  passcode_hash  TEXT,
  max_files      INTEGER NOT NULL,
  max_file_bytes INTEGER NOT NULL,
  version        INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX links_expires_at ON links (expires_at);
CREATE INDEX links_ended_at ON links (ended_at) WHERE ended_at IS NOT NULL;

-- A file is a compact JWE, which is text. The host holds no key to open it.
CREATE TABLE link_files (
  link_id      TEXT NOT NULL REFERENCES links (id) ON DELETE CASCADE,
  seq          INTEGER NOT NULL,
  name         TEXT NOT NULL,
  content_type TEXT NOT NULL,
  bytes        INTEGER NOT NULL,
  body         TEXT NOT NULL,
  PRIMARY KEY (link_id, seq),
  UNIQUE (link_id, name)
) STRICT;

CREATE TABLE link_access (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  link_id    TEXT NOT NULL REFERENCES links (id) ON DELETE CASCADE,
  at         INTEGER NOT NULL,
  recipient  TEXT NOT NULL,
  outcome    TEXT NOT NULL,
  ip         TEXT,
  user_agent TEXT
) STRICT;
CREATE INDEX link_access_link ON link_access (link_id, id);

-- Secrets the host makes for itself, so that every process agrees on them.
CREATE TABLE host_secrets (
  name  TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;

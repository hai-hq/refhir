-- Links, the encrypted files on them, and who has asked. Times are seconds since 1970.
CREATE TABLE links (
  id             text PRIMARY KEY,
  token_hash     text NOT NULL,
  active         boolean NOT NULL DEFAULT true,
  created_at     bigint NOT NULL,
  created_by     text NOT NULL,
  expires_at     bigint NOT NULL,
  ended_at       bigint,
  attempts_left  integer NOT NULL,
  passcode_salt  text,
  passcode_hash  text,
  max_files      integer NOT NULL,
  max_file_bytes integer NOT NULL
);
CREATE INDEX links_expires_at ON links (expires_at);
CREATE INDEX links_ended_at ON links (ended_at) WHERE ended_at IS NOT NULL;

-- A file is a compact JWE, which is text. The host holds no key to open it.
CREATE TABLE link_files (
  link_id      text NOT NULL REFERENCES links (id) ON DELETE CASCADE,
  seq          integer NOT NULL,
  name         text NOT NULL,
  content_type text NOT NULL,
  bytes        integer NOT NULL,
  body         text NOT NULL,
  PRIMARY KEY (link_id, seq),
  UNIQUE (link_id, name)
);

CREATE TABLE link_access (
  id         bigserial PRIMARY KEY,
  link_id    text NOT NULL REFERENCES links (id) ON DELETE CASCADE,
  at         bigint NOT NULL,
  recipient  text NOT NULL,
  outcome    text NOT NULL,
  ip         text,
  user_agent text
);
CREATE INDEX link_access_link ON link_access (link_id, id);

-- Secrets the host makes for itself, so that every process agrees on them.
CREATE TABLE host_secrets (
  name  text PRIMARY KEY,
  value text NOT NULL
);

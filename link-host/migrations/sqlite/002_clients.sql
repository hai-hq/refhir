-- Clients made over the admin API, and whose each link is. A client's settings (its limits, the
-- addresses its token is accepted from, the media types its links may hold) are JSON, as the API
-- says them less the lifetimes, which are in seconds.
CREATE TABLE link_clients (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL UNIQUE,
  token_hash TEXT NOT NULL UNIQUE,
  settings   TEXT NOT NULL DEFAULT '{}',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

-- A link is its client's: the client's id, or the name of a client the host was given in its settings,
-- which is what every link made before this said it was made by.
ALTER TABLE links ADD COLUMN client_id TEXT;
ALTER TABLE links ADD COLUMN content_types TEXT;
UPDATE links SET client_id = created_by;
CREATE INDEX links_client ON links (client_id, created_at DESC, id DESC);

-- A host that has not been upgraded yet makes links with no client_id: they are the client's that made
-- them, as the ones above are.
CREATE TRIGGER links_client_default AFTER INSERT ON links WHEN NEW.client_id IS NULL
BEGIN
  UPDATE links SET client_id = NEW.created_by WHERE id = NEW.id;
END;

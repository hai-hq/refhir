use refhir_persistence::{auth_exp, AuthStore, PersistError};
use rusqlite::{params, OptionalExtension};

use crate::{definite, SqlitePersistence};

impl AuthStore for SqlitePersistence {
    fn auth_put(&self, kind: &str, id: &str, json: &str, exp: u64) -> Result<(), PersistError> {
        let guard = self.inner.lock()?;
        guard
            .connection
            .execute(
                "INSERT OR REPLACE INTO smart_oauth (kind, id, json, exp) VALUES (?1, ?2, ?3, ?4)",
                params![kind, id, json, auth_exp(exp)],
            )
            .map_err(definite)?;
        Ok(())
    }

    fn auth_insert(
        &self,
        kind: &str,
        id: &str,
        json: &str,
        exp: u64,
        now: u64,
    ) -> Result<bool, PersistError> {
        let guard = self.inner.lock()?;
        let changed = guard
            .connection
            .execute(
                "INSERT INTO smart_oauth (kind, id, json, exp) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (kind, id) DO UPDATE SET json = excluded.json, exp = excluded.exp \
                 WHERE smart_oauth.exp <= ?5",
                params![kind, id, json, auth_exp(exp), auth_exp(now)],
            )
            .map_err(definite)?;
        Ok(changed == 1)
    }

    fn auth_get(&self, kind: &str, id: &str, now: u64) -> Result<Option<String>, PersistError> {
        let guard = self.read()?;
        guard
            .connection
            .query_row(
                "SELECT json FROM smart_oauth WHERE kind = ?1 AND id = ?2 AND exp > ?3",
                params![kind, id, auth_exp(now)],
                |row| row.get(0),
            )
            .optional()
            .map_err(definite)
    }

    fn auth_take(&self, kind: &str, id: &str, now: u64) -> Result<Option<String>, PersistError> {
        let guard = self.inner.lock()?;
        let row: Option<(String, i64)> = guard
            .connection
            .query_row(
                "DELETE FROM smart_oauth WHERE kind = ?1 AND id = ?2 RETURNING json, exp",
                params![kind, id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(definite)?;
        Ok(row
            .filter(|(_, exp)| *exp > auth_exp(now))
            .map(|(json, _)| json))
    }

    fn auth_delete_if(&self, kind: &str, id: &str, json: &str) -> Result<bool, PersistError> {
        let guard = self.inner.lock()?;
        let deleted = guard
            .connection
            .execute(
                "DELETE FROM smart_oauth WHERE kind = ?1 AND id = ?2 AND json = ?3",
                params![kind, id, json],
            )
            .map_err(definite)?;
        Ok(deleted == 1)
    }

    fn auth_list(&self, kind: &str, now: u64) -> Result<Vec<String>, PersistError> {
        let guard = self.read()?;
        let mut stmt = guard
            .connection
            .prepare("SELECT json FROM smart_oauth WHERE kind = ?1 AND exp > ?2 ORDER BY id")
            .map_err(definite)?;
        let rows = stmt
            .query_map(params![kind, auth_exp(now)], |row| row.get(0))
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?;
        Ok(rows)
    }

    fn auth_prune(&self, now: u64) -> Result<(), PersistError> {
        let guard = self.inner.lock()?;
        guard
            .connection
            .execute(
                "DELETE FROM smart_oauth WHERE exp <= ?1",
                params![auth_exp(now)],
            )
            .map_err(definite)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_auth_store() {
        let store = SqlitePersistence::open_in_memory().unwrap();
        refhir_persistence::auth::exercise(&store);
    }
}

use refhir_persistence::{auth_exp, AuthStore, PersistError};

use crate::{definite, PostgresPersistence};

impl AuthStore for PostgresPersistence {
    fn auth_put(&self, kind: &str, id: &str, json: &str, exp: u64) -> Result<(), PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            guard
                .client
                .execute(
                    &format!(
                        "INSERT INTO smart_oauth ({}kind, id, json, exp) VALUES ({}$1, $2, $3, $4) \
                         ON CONFLICT {} DO UPDATE SET json = excluded.json, exp = excluded.exp",
                        self.scope.cols(),
                        self.scope.vals(),
                        self.scope.conflict("kind, id")
                    ),
                    &[&kind, &id, &json, &auth_exp(exp)],
                )
                .map_err(definite)?;
            Ok(())
        })
    }

    fn auth_insert(
        &self,
        kind: &str,
        id: &str,
        json: &str,
        exp: u64,
        now: u64,
    ) -> Result<bool, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let changed = guard
                .client
                .execute(
                    &format!(
                        "INSERT INTO smart_oauth ({}kind, id, json, exp) VALUES ({}$1, $2, $3, $4) \
                         ON CONFLICT {} DO UPDATE SET json = excluded.json, exp = excluded.exp \
                         WHERE smart_oauth.exp <= $5",
                        self.scope.cols(),
                        self.scope.vals(),
                        self.scope.conflict("kind, id")
                    ),
                    &[&kind, &id, &json, &auth_exp(exp), &auth_exp(now)],
                )
                .map_err(definite)?;
            Ok(changed == 1)
        })
    }

    fn auth_get(&self, kind: &str, id: &str, now: u64) -> Result<Option<String>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let row = guard
                .client
                .query_opt(
                    &format!(
                        "SELECT json FROM smart_oauth WHERE {}kind = $1 AND id = $2 AND exp > $3",
                        self.scope.and()
                    ),
                    &[&kind, &id, &auth_exp(now)],
                )
                .map_err(definite)?;
            Ok(row.map(|row| row.get(0)))
        })
    }

    fn auth_take(&self, kind: &str, id: &str, now: u64) -> Result<Option<String>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let row = guard
                .client
                .query_opt(
                    &format!(
                        "DELETE FROM smart_oauth WHERE {}kind = $1 AND id = $2 RETURNING json, exp",
                        self.scope.and()
                    ),
                    &[&kind, &id],
                )
                .map_err(definite)?;
            Ok(row
                .filter(|row| row.get::<_, i64>(1) > auth_exp(now))
                .map(|row| row.get(0)))
        })
    }

    fn auth_delete_if(&self, kind: &str, id: &str, json: &str) -> Result<bool, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let deleted = guard
                .client
                .execute(
                    &format!(
                        "DELETE FROM smart_oauth WHERE {}kind = $1 AND id = $2 AND json = $3",
                        self.scope.and()
                    ),
                    &[&kind, &id, &json],
                )
                .map_err(definite)?;
            Ok(deleted == 1)
        })
    }

    fn auth_list(&self, kind: &str, now: u64) -> Result<Vec<String>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let rows = guard
                .client
                .query(
                    &format!(
                        "SELECT json FROM smart_oauth WHERE {}kind = $1 AND exp > $2 ORDER BY id",
                        self.scope.and()
                    ),
                    &[&kind, &auth_exp(now)],
                )
                .map_err(definite)?;
            Ok(rows.iter().map(|row| row.get(0)).collect())
        })
    }

    fn auth_prune(&self, now: u64) -> Result<(), PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            guard
                .client
                .execute(
                    &format!(
                        "DELETE FROM smart_oauth WHERE {}exp <= $1",
                        self.scope.and()
                    ),
                    &[&auth_exp(now)],
                )
                .map_err(definite)?;
            Ok(())
        })
    }
}

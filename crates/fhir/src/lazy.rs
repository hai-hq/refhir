//! Reading the conformance tables when a definition or a code system is first used.
//!
//! A catalog built from a store that can give a handle to itself keeps an index of what the
//! tables hold, and reads a row when a request needs what it says. The rows are the ones the
//! catalog was built from. A server whose catalog moves reloads it, and a catalog that was
//! replaced takes what it read with it.

use std::sync::Arc;

use refhir_persistence::{ConformanceStore, PersistError};
use refhir_validate::{DefinitionSource, Scope, TermSource};
use serde_json::Value;

/// The conformance tables of a store, read a row at a time.
pub(crate) struct StoreSource {
    store: Arc<dyn ConformanceStore>,
}

impl StoreSource {
    pub(crate) fn new(store: Arc<dyn ConformanceStore>) -> Self {
        Self { store }
    }

    /// The row as JSON. A row that cannot be read is logged and answers as if it were not there:
    /// a question about a definition or a code system has no way to say the store failed.
    fn parse(
        what: &str,
        package: i64,
        key: &str,
        row: Result<Option<String>, PersistError>,
    ) -> Option<Value> {
        let text = match row {
            Ok(text) => text?,
            Err(err) => {
                tracing::error!(
                    target: "refhir_engine",
                    "could not read {what} {key} of package {package}: {err}"
                );
                return None;
            }
        };
        match serde_json::from_str(&text) {
            Ok(value) => Some(value),
            Err(err) => {
                tracing::error!(
                    target: "refhir_engine",
                    "{what} {key} of package {package} is not JSON: {err}"
                );
                None
            }
        }
    }
}

impl DefinitionSource for StoreSource {
    fn definition(&self, package: i64, url: &str) -> Option<Value> {
        Self::parse(
            "structure definition",
            package,
            url,
            self.store.load_structure_definition_json(package, url),
        )
    }
}

impl TermSource for StoreSource {
    fn resource(&self, package: Scope, key: &str) -> Option<Value> {
        Self::parse(
            "terminology resource",
            package,
            key,
            self.store.load_terminology_json(package, key),
        )
    }

    fn expansion(&self, package: Scope, key: &str) -> Option<Value> {
        Self::parse(
            "value set expansion",
            package,
            key,
            self.store.load_expansion_json(package, key),
        )
    }
}

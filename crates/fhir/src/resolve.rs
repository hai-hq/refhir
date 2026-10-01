//! `resolve()` for search indexing. Contained references stay inside FHIRPath.
//! `Type/id` is a point read on the open transaction.

use std::cell::RefCell;

use refhir_database::Transaction;
use refhir_fhirpath::{Error, Host};
use serde_json::Value as Json;

use crate::catalog::Catalog;

pub struct TxHost<'a> {
    tx: RefCell<&'a mut Transaction>,
    catalog: &'a Catalog,
}

impl<'a> TxHost<'a> {
    pub fn new(tx: &'a mut Transaction, catalog: &'a Catalog) -> Self {
        Self {
            tx: RefCell::new(tx),
            catalog,
        }
    }
}

impl Host for TxHost<'_> {
    fn resolve(&self, reference: &str) -> Result<Option<Json>, Error> {
        let Some((type_name, id)) = split_reference(reference) else {
            return Ok(None);
        };
        let Ok(model) = self.catalog.get(type_name) else {
            return Ok(None);
        };
        let stored = self
            .tx
            .borrow_mut()
            .get(&model.tablet(), id, &model.by_id())
            .map_err(|err| Error::eval(err.to_string()))?;
        Ok(stored.map(|stored| stored.json))
    }
}

fn split_reference(reference: &str) -> Option<(&str, &str)> {
    let mut parts = reference.split('/').rev();
    let id = parts.next()?;
    let type_name = parts.next()?;
    if id.is_empty() || type_name.is_empty() || !type_name.chars().next()?.is_ascii_uppercase() {
        return None;
    }
    Some((type_name, id))
}

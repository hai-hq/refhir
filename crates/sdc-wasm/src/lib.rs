//! WebAssembly export of the questionnaire behavior engine.
//!
//! The browser filler loads this module when it has been built for
//! `wasm32-unknown-unknown`. The same `apply_behavior` function runs on the
//! server at `Questionnaire/$behavior`, so the two paths share one implementation.

use wasm_bindgen::prelude::*;

use refhir_sdc::{apply_behavior, NullChart};

#[wasm_bindgen]
pub fn behavior_json(questionnaire: &str, response: &str) -> String {
    let questionnaire = serde_json::from_str(questionnaire).unwrap_or(serde_json::json!({}));
    let response = serde_json::from_str(response).unwrap_or_else(|_| {
        serde_json::json!({
            "resourceType": "QuestionnaireResponse",
            "status": "in-progress"
        })
    });
    match apply_behavior(&mut NullChart, &questionnaire, &response) {
        Ok(value) => value.to_string(),
        Err(err) => serde_json::json!({"resourceType": "OperationOutcome", "issue": [{"severity": "error", "diagnostics": err}]}).to_string(),
    }
}

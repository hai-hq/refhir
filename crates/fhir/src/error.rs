use refhir_persistence::PersistError;
use serde_json::Value;

#[derive(Debug)]
pub enum FhirError {
    NotFound(String),
    Gone(String),
    Conflict(String),
    Precondition(String),
    Invalid(String),
    Outcome(Value),
    UnknownResource(String),
    Unsupported(String),
    Storage(String),
    /// The caller is not allowed to perform the interaction.
    Forbidden(String),
}

impl std::fmt::Display for FhirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FhirError::NotFound(message)
            | FhirError::Gone(message)
            | FhirError::Conflict(message)
            | FhirError::Precondition(message)
            | FhirError::Invalid(message)
            | FhirError::UnknownResource(message)
            | FhirError::Unsupported(message)
            | FhirError::Storage(message)
            | FhirError::Forbidden(message) => write!(f, "{message}"),
            FhirError::Outcome(value) => {
                let diagnostics = value
                    .pointer("/issue/0/diagnostics")
                    .and_then(|item| item.as_str())
                    .unwrap_or("validation failed");
                write!(f, "{diagnostics}")
            }
        }
    }
}

impl std::error::Error for FhirError {}

impl From<PersistError> for FhirError {
    fn from(err: PersistError) -> Self {
        FhirError::Storage(err.to_string())
    }
}

pub fn outcome_code(err: &FhirError) -> &'static str {
    match err {
        FhirError::NotFound(_) | FhirError::UnknownResource(_) => "not-found",
        FhirError::Gone(_) => "deleted",
        FhirError::Conflict(_) | FhirError::Precondition(_) => "conflict",
        FhirError::Invalid(_) | FhirError::Outcome(_) | FhirError::Unsupported(_) => "invalid",
        FhirError::Storage(_) => "exception",
        FhirError::Forbidden(_) => "forbidden",
    }
}

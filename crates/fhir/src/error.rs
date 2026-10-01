use refhir_persistence::PersistError;

#[derive(Debug)]
pub enum FhirError {
    NotFound(String),
    Gone(String),
    Conflict(String),
    Invalid(String),
    UnknownResource(String),
    Unsupported(String),
    Storage(String),
}

impl std::fmt::Display for FhirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FhirError::NotFound(message)
            | FhirError::Gone(message)
            | FhirError::Conflict(message)
            | FhirError::Invalid(message)
            | FhirError::UnknownResource(message)
            | FhirError::Unsupported(message)
            | FhirError::Storage(message) => write!(f, "{message}"),
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
        FhirError::Conflict(_) => "conflict",
        FhirError::Invalid(_) | FhirError::Unsupported(_) => "invalid",
        FhirError::Storage(_) => "exception",
    }
}

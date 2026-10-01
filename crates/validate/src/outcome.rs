use serde_json::{json, Value as Json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Fatal,
    Error,
    Warning,
    Information,
}

impl Severity {
    pub fn parse(text: &str) -> Self {
        match text {
            "fatal" => Severity::Fatal,
            "warning" => Severity::Warning,
            "information" => Severity::Information,
            _ => Severity::Error,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Fatal => "fatal",
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Information => "information",
        }
    }

    pub fn fails(self) -> bool {
        matches!(self, Severity::Fatal | Severity::Error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub severity: Severity,
    pub code: String,
    pub diagnostics: String,
    pub expression: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub issues: Vec<Issue>,
}

impl Outcome {
    pub fn has_errors(&self) -> bool {
        self.issues.iter().any(|issue| issue.severity.fails())
    }

    pub fn operation_outcome(&self) -> Json {
        json!({
            "resourceType": "OperationOutcome",
            "issue": if self.issues.is_empty() {
                vec![json!({
                    "severity": "information",
                    "code": "informational",
                    "diagnostics": "validation succeeded"
                })]
            } else {
                self.issues.iter().map(|issue| {
                    json!({
                        "severity": issue.severity.as_str(),
                        "code": issue.code,
                        "diagnostics": issue.diagnostics,
                        "expression": [issue.expression]
                    })
                }).collect()
            }
        })
    }
}

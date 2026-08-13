//! Capability labels shared by task requirements and MCP sessions.
//!
//! Labels are deliberately small, human-controlled tokens rather than a
//! taxonomy hird owns. A task may require `browser` or `macos`; a harness
//! registration advertises the same words through [`CAPABILITIES_ENV`].

use std::collections::BTreeSet;

use crate::error::{Error, Result};

/// Comma-separated capabilities available to one MCP server process.
pub const CAPABILITIES_ENV: &str = "HIRD_CAPABILITIES";

/// Normalize, validate, sort and deduplicate capability labels.
pub fn normalize_all<I, S>(labels: I) -> Result<Vec<String>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut normalized = BTreeSet::new();
    for raw in labels {
        let label = raw.as_ref().trim().to_ascii_lowercase();
        if label.is_empty() {
            return Err(Error::invalid("a capability label must not be empty"));
        }
        if label.len() > 48 {
            return Err(Error::invalid(format!(
                "capability {label:?} is too long; keep labels to 48 characters"
            )));
        }
        if !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(Error::invalid(format!(
                "capability {label:?} contains an unsupported character; use letters, numbers, '.', '-' or '_'"
            )));
        }
        normalized.insert(label);
    }
    Ok(normalized.into_iter().collect())
}

/// Read this process's human-controlled capability labels.
pub fn from_env() -> Result<Vec<String>> {
    let raw = std::env::var(CAPABILITIES_ENV).unwrap_or_default();
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    normalize_all(raw.split(','))
}

/// Requirements in `required` that `available` does not satisfy.
pub fn missing(required: &[String], available: &[String]) -> Vec<String> {
    required
        .iter()
        .filter(|required| available.binary_search(required).is_err())
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_normalized_sorted_and_deduplicated() {
        assert_eq!(
            normalize_all([" Browser ", "network", "browser"]).unwrap(),
            vec!["browser", "network"]
        );
    }

    #[test]
    fn labels_are_shell_safe_tokens() {
        assert!(normalize_all(["browser,network"]).is_err());
        assert!(normalize_all(["has space"]).is_err());
        assert!(normalize_all(["macos-14", "gpu.cuda"]).is_ok());
    }

    #[test]
    fn missing_reports_only_unsatisfied_requirements() {
        assert_eq!(
            missing(&["browser".into(), "network".into()], &["browser".into()]),
            vec!["network"]
        );
    }
}

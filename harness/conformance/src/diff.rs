use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::oracle::OracleState;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DivergenceAllowlist {
    #[serde(default)]
    pub divergence: Vec<Divergence>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Divergence {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDifference {
    pub path: String,
    pub expected: Value,
    pub actual: Value,
}

impl DivergenceAllowlist {
    pub fn load(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path).with_context(|| format!("read divergence allowlist {}", path.display()))?;
        toml::from_str(&source).context("parse divergence allowlist")
    }

    fn permits(&self, path: &str) -> bool {
        self.divergence.iter().any(|entry| entry.path == path)
    }
}

pub fn compare_states(expected: &OracleState, actual: &OracleState, allowlist: &DivergenceAllowlist) -> Result<Vec<StateDifference>> {
    let expected = serde_json::to_value(expected)?;
    let actual = serde_json::to_value(actual)?;
    let mut paths = BTreeSet::new();
    collect_paths("$", &expected, &mut paths);
    collect_paths("$", &actual, &mut paths);
    let differences = paths
        .into_iter()
        .filter(|path| !allowlist.permits(path))
        .filter_map(|path| {
            let expected_value = pointer(&expected, &path);
            let actual_value = pointer(&actual, &path);
            (expected_value != actual_value).then(|| StateDifference {
                path,
                expected: expected_value.cloned().unwrap_or(Value::Null),
                actual: actual_value.cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    Ok(differences)
}

fn collect_paths(prefix: &str, value: &Value, output: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let path = format!("{prefix}/{key}");
                output.insert(path.clone());
                collect_paths(&path, value, output);
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                let path = format!("{prefix}/{index}");
                output.insert(path.clone());
                collect_paths(&path, value, output);
            }
        }
        _ => {}
    }
}

fn pointer<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    value.pointer(path.strip_prefix('$').unwrap_or(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_suppresses_exact_paths() {
        let value = OracleState {
            mode: "n".to_owned(),
            pending_operator: String::new(),
            buffer: vec!["x".to_owned()],
            cursor: Value::Null,
            selections: Value::Null,
            registers: Value::Null,
            marks: Value::Null,
            jumplist: Value::Null,
            changelist: Value::Null,
            search: Value::Null,
            messages: String::new(),
            undo_tree: Value::Null,
            options: Value::Null,
        };
        let mut changed = value.clone();
        changed.mode = "i".to_owned();
        let allowlist = DivergenceAllowlist { divergence: vec![Divergence { path: "$/mode".to_owned(), reason: "test".to_owned() }] };
        assert!(compare_states(&value, &changed, &allowlist).expect("comparison").is_empty());
    }
}

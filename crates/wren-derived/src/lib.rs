#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;
use std::sync::Arc;

use wren_types::{
    ConfigGeneration, DocumentId, DocumentRevision, ProviderGeneration, WorkspaceGeneration,
    WorkspaceGenerationKind,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentInput {
    pub revision: DocumentRevision,
    pub language_id: Box<str>,
    pub text: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineItem {
    pub name: Box<str>,
    pub byte: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSymbol {
    pub name: Box<str>,
    pub document_id: DocumentId,
    pub byte: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResult<T> {
    pub value: Arc<T>,
    pub computed_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct OutlineKey {
    document_id: DocumentId,
    revision: DocumentRevision,
    provider_generation: ProviderGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SymbolsKey {
    generation: WorkspaceGeneration,
    provider_generation: ProviderGeneration,
}

#[derive(Debug, Default)]
pub struct DerivedStateDb {
    documents: BTreeMap<DocumentId, DocumentInput>,
    workspace_generations: BTreeMap<WorkspaceGenerationKind, WorkspaceGeneration>,
    provider_generations: BTreeMap<Box<str>, ProviderGeneration>,
    config_generation: Option<ConfigGeneration>,
    outline_cache: BTreeMap<OutlineKey, QueryResult<Vec<OutlineItem>>>,
    symbol_cache: BTreeMap<SymbolsKey, QueryResult<Vec<WorkspaceSymbol>>>,
    query_clock: u64,
}

impl DerivedStateDb {
    pub fn set_document(&mut self, document_id: DocumentId, input: DocumentInput) {
        self.documents.insert(document_id, input);
    }

    pub fn set_workspace_generation(
        &mut self,
        kind: WorkspaceGenerationKind,
        generation: WorkspaceGeneration,
    ) {
        self.workspace_generations.insert(kind, generation);
    }

    pub fn set_provider_generation(
        &mut self,
        provider: impl Into<Box<str>>,
        generation: ProviderGeneration,
    ) {
        self.provider_generations
            .insert(provider.into(), generation);
    }

    pub fn set_config_generation(&mut self, generation: ConfigGeneration) {
        self.config_generation = Some(generation);
    }

    #[must_use]
    pub fn resolved_language(&self, document_id: DocumentId) -> Option<&str> {
        self.documents
            .get(&document_id)
            .map(|document| document.language_id.as_ref())
    }

    #[must_use]
    pub fn command_enabled(&self, command: &str, trusted: bool) -> bool {
        let configured = self.config_generation.is_some();
        if command.starts_with("task.") || command.starts_with("extension.") {
            configured && trusted
        } else {
            configured
        }
    }

    pub fn outline(&mut self, document_id: DocumentId) -> Option<QueryResult<Vec<OutlineItem>>> {
        let document = self.documents.get(&document_id)?;
        let generation = self
            .provider_generations
            .get("syntax")
            .copied()
            .unwrap_or(ProviderGeneration::new(0));
        let key = OutlineKey {
            document_id,
            revision: document.revision,
            provider_generation: generation,
        };
        if let Some(cached) = self.outline_cache.get(&key) {
            return Some(cached.clone());
        }
        let value = Arc::new(compute_outline(&document.text));
        let result = QueryResult {
            value,
            computed_at: self.tick(),
        };
        self.outline_cache.insert(key, result.clone());
        Some(result)
    }

    pub fn workspace_symbols(&mut self) -> QueryResult<Vec<WorkspaceSymbol>> {
        let generation = self
            .workspace_generations
            .get(&WorkspaceGenerationKind::ProjectIndex)
            .copied()
            .unwrap_or(WorkspaceGeneration::new(0));
        let provider_generation = self
            .provider_generations
            .get("project-index")
            .copied()
            .unwrap_or(ProviderGeneration::new(0));
        let key = SymbolsKey {
            generation,
            provider_generation,
        };
        if let Some(cached) = self.symbol_cache.get(&key) {
            return cached.clone();
        }
        let mut value = Vec::new();
        for (document_id, document) in &self.documents {
            value.extend(
                compute_outline(&document.text)
                    .into_iter()
                    .map(|item| WorkspaceSymbol {
                        name: item.name,
                        document_id: *document_id,
                        byte: item.byte,
                    }),
            );
        }
        value.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.document_id.cmp(&right.document_id))
        });
        let result = QueryResult {
            value: Arc::new(value),
            computed_at: self.tick(),
        };
        self.symbol_cache.insert(key, result.clone());
        result
    }

    #[must_use]
    pub fn effective_keymap_generation(&self) -> Option<ConfigGeneration> {
        self.config_generation
    }

    #[must_use]
    pub fn git_baseline_generation(&self) -> Option<WorkspaceGeneration> {
        self.workspace_generations
            .get(&WorkspaceGenerationKind::Git)
            .copied()
    }

    fn tick(&mut self) -> u64 {
        self.query_clock = self.query_clock.saturating_add(1);
        self.query_clock
    }
}

fn compute_outline(text: &str) -> Vec<OutlineItem> {
    let mut items = Vec::new();
    let mut byte = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let indentation = line.len().saturating_sub(trimmed.len());
        for prefix in ["fn ", "struct ", "enum ", "trait ", "class "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let name = rest
                    .split(|character: char| !character.is_alphanumeric() && character != '_')
                    .next()
                    .unwrap_or_default();
                if !name.is_empty() {
                    items.push(OutlineItem {
                        name: name.into(),
                        byte: byte + indentation + prefix.len(),
                    });
                }
                break;
            }
        }
        byte += line.len();
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_document_b_does_not_invalidate_document_a_queries() {
        let mut db = DerivedStateDb::default();
        db.set_provider_generation("syntax", ProviderGeneration::new(1));
        db.set_document(
            DocumentId::new(1),
            DocumentInput {
                revision: DocumentRevision::new(1),
                language_id: "rust".into(),
                text: Arc::from("fn alpha() {}"),
            },
        );
        db.set_document(
            DocumentId::new(2),
            DocumentInput {
                revision: DocumentRevision::new(1),
                language_id: "rust".into(),
                text: Arc::from("fn beta() {}"),
            },
        );
        let alpha = db.outline(DocumentId::new(1)).expect("alpha");
        db.set_document(
            DocumentId::new(2),
            DocumentInput {
                revision: DocumentRevision::new(2),
                language_id: "rust".into(),
                text: Arc::from("fn beta_changed() {}"),
            },
        );
        let alpha_again = db.outline(DocumentId::new(1)).expect("alpha cached");
        assert_eq!(alpha.computed_at, alpha_again.computed_at);
        assert!(Arc::ptr_eq(&alpha.value, &alpha_again.value));
    }

    #[test]
    fn provider_and_domain_generations_invalidate_only_their_query_shapes() {
        let mut db = DerivedStateDb::default();
        db.set_document(
            DocumentId::new(1),
            DocumentInput {
                revision: DocumentRevision::new(1),
                language_id: "rust".into(),
                text: Arc::from("fn alpha() {}"),
            },
        );
        let outline = db.outline(DocumentId::new(1)).expect("outline");
        db.set_workspace_generation(WorkspaceGenerationKind::Git, WorkspaceGeneration::new(2));
        assert_eq!(
            outline.computed_at,
            db.outline(DocumentId::new(1))
                .expect("outline retained")
                .computed_at
        );
        db.set_provider_generation("syntax", ProviderGeneration::new(2));
        assert_ne!(
            outline.computed_at,
            db.outline(DocumentId::new(1))
                .expect("outline refreshed")
                .computed_at
        );
    }

    #[test]
    fn trust_sensitive_command_enabled_state_is_a_coarse_query() {
        let mut db = DerivedStateDb::default();
        db.set_config_generation(ConfigGeneration::new(4));
        assert!(db.command_enabled("picker.files", false));
        assert!(!db.command_enabled("task.build", false));
        assert!(db.command_enabled("task.build", true));
    }
}

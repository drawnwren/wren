use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use thiserror::Error;
use wren_types::{DocumentId, DocumentRevision};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotQuota {
    pub max_bytes: usize,
    pub max_revisions: usize,
    pub held_too_long: Duration,
}

impl Default for SnapshotQuota {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1_048_576,
            max_revisions: 8,
            held_too_long: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldSnapshot {
    pub provider: Box<str>,
    pub document_id: DocumentId,
    pub revision: DocumentRevision,
    pub bytes: usize,
    pub age: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMetrics {
    pub live_revisions: usize,
    pub retained_snapshot_bytes: usize,
    pub oldest_live_revision: Option<DocumentRevision>,
    pub held_too_long: Vec<HeldSnapshot>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    #[error(
        "provider {provider} snapshot quota would exceed {max_bytes} bytes (currently {current_bytes}, requested {requested_bytes})"
    )]
    ByteQuota {
        provider: Box<str>,
        current_bytes: usize,
        requested_bytes: usize,
        max_bytes: usize,
    },
    #[error("provider {provider} snapshot quota would exceed {max_revisions} live revisions")]
    RevisionQuota {
        provider: Box<str>,
        max_revisions: usize,
    },
    #[error("snapshot range {start}..{end} is outside {len} bytes or not UTF-8 aligned")]
    InvalidRange {
        start: usize,
        end: usize,
        len: usize,
    },
    #[error("snapshot manager lock was poisoned")]
    Poisoned,
}

#[derive(Debug)]
struct SnapshotRecord {
    provider: Box<str>,
    document_id: DocumentId,
    revision: DocumentRevision,
    bytes: usize,
    issued_at: Instant,
    text: Arc<str>,
}

/// Opaque, quota-accounted provider snapshot. Callers can inspect text only
/// through borrowed callbacks or owned slices; the backing allocation cannot
/// be extracted and retained after the handle is released.
#[derive(Debug, Clone)]
pub struct SnapshotHandle {
    record: Arc<SnapshotRecord>,
}

impl SnapshotHandle {
    #[must_use]
    pub fn document_id(&self) -> DocumentId {
        self.record.document_id
    }

    #[must_use]
    pub fn revision(&self) -> DocumentRevision {
        self.record.revision
    }

    #[must_use]
    pub fn len_bytes(&self) -> usize {
        self.record.bytes
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.record.text.is_empty()
    }

    pub fn with_text<T>(&self, read: impl FnOnce(&str) -> T) -> T {
        read(&self.record.text)
    }

    pub fn slice(&self, range: Range<usize>) -> Result<String, SnapshotError> {
        self.record
            .text
            .get(range.clone())
            .map(ToOwned::to_owned)
            .ok_or(SnapshotError::InvalidRange {
                start: range.start,
                end: range.end,
                len: self.record.bytes,
            })
    }
}

#[derive(Debug)]
struct ManagerState {
    default_quota: SnapshotQuota,
    provider_quotas: BTreeMap<Box<str>, SnapshotQuota>,
    records: Vec<Weak<SnapshotRecord>>,
}

impl ManagerState {
    fn prune(&mut self) {
        self.records.retain(|record| record.strong_count() > 0);
    }

    fn quota_for(&self, provider: &str) -> SnapshotQuota {
        self.provider_quotas
            .get(provider)
            .copied()
            .unwrap_or(self.default_quota)
    }
}

/// Issues provider snapshots and centralizes retention accounting.
#[derive(Debug, Clone)]
pub struct SnapshotManager {
    state: Arc<Mutex<ManagerState>>,
}

impl SnapshotManager {
    #[must_use]
    pub fn new(default_quota: SnapshotQuota) -> Self {
        Self {
            state: Arc::new(Mutex::new(ManagerState {
                default_quota,
                provider_quotas: BTreeMap::new(),
                records: Vec::new(),
            })),
        }
    }

    pub fn set_provider_quota(
        &self,
        provider: impl Into<Box<str>>,
        quota: SnapshotQuota,
    ) -> Result<(), SnapshotError> {
        self.state
            .lock()
            .map_err(|_| SnapshotError::Poisoned)?
            .provider_quotas
            .insert(provider.into(), quota);
        Ok(())
    }

    pub fn issue(
        &self,
        provider: impl Into<Box<str>>,
        document_id: DocumentId,
        revision: DocumentRevision,
        text: impl Into<Arc<str>>,
    ) -> Result<SnapshotHandle, SnapshotError> {
        let provider = provider.into();
        let text = text.into();
        let requested_bytes = text.len();
        let mut state = self.state.lock().map_err(|_| SnapshotError::Poisoned)?;
        state.prune();
        let quota = state.quota_for(&provider);
        let live = state
            .records
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|record| record.provider == provider)
            .collect::<Vec<_>>();
        let current_bytes = live
            .iter()
            .try_fold(0_usize, |total, record| total.checked_add(record.bytes));
        let current_bytes = current_bytes.unwrap_or(usize::MAX);
        if current_bytes.saturating_add(requested_bytes) > quota.max_bytes {
            return Err(SnapshotError::ByteQuota {
                provider,
                current_bytes,
                requested_bytes,
                max_bytes: quota.max_bytes,
            });
        }
        if live.len() >= quota.max_revisions {
            return Err(SnapshotError::RevisionQuota {
                provider,
                max_revisions: quota.max_revisions,
            });
        }
        let record = Arc::new(SnapshotRecord {
            provider,
            document_id,
            revision,
            bytes: requested_bytes,
            issued_at: Instant::now(),
            text,
        });
        state.records.push(Arc::downgrade(&record));
        Ok(SnapshotHandle { record })
    }

    pub fn metrics(&self) -> Result<SnapshotMetrics, SnapshotError> {
        let mut state = self.state.lock().map_err(|_| SnapshotError::Poisoned)?;
        state.prune();
        let now = Instant::now();
        let live = state
            .records
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        let retained_snapshot_bytes = live
            .iter()
            .fold(0_usize, |total, record| total.saturating_add(record.bytes));
        let oldest_live_revision = live.iter().map(|record| record.revision).min();
        let held_too_long = live
            .iter()
            .filter_map(|record| {
                let age = now.saturating_duration_since(record.issued_at);
                (age >= state.quota_for(&record.provider).held_too_long).then(|| HeldSnapshot {
                    provider: record.provider.clone(),
                    document_id: record.document_id,
                    revision: record.revision,
                    bytes: record.bytes,
                    age,
                })
            })
            .collect();
        Ok(SnapshotMetrics {
            live_revisions: live.len(),
            retained_snapshot_bytes,
            oldest_live_revision,
            held_too_long,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota() -> SnapshotQuota {
        SnapshotQuota {
            max_bytes: 10,
            max_revisions: 2,
            held_too_long: Duration::ZERO,
        }
    }

    #[test]
    fn quotas_are_provider_scoped_and_drop_releases_retention() {
        let manager = SnapshotManager::new(quota());
        let first = manager
            .issue(
                "syntax",
                DocumentId::new(1),
                DocumentRevision::new(2),
                "12345",
            )
            .expect("first snapshot");
        let clone = first.clone();
        let second = manager
            .issue(
                "syntax",
                DocumentId::new(1),
                DocumentRevision::new(3),
                "67890",
            )
            .expect("second snapshot");
        assert!(matches!(
            manager.issue("syntax", DocumentId::new(1), DocumentRevision::new(4), "x"),
            Err(SnapshotError::ByteQuota { .. })
        ));
        let other = manager
            .issue("git", DocumentId::new(1), DocumentRevision::new(4), "x")
            .expect("other provider has its own quota");
        assert_eq!(manager.metrics().expect("metrics").live_revisions, 3);
        drop(first);
        assert_eq!(manager.metrics().expect("clone retains").live_revisions, 3);
        drop(clone);
        drop(second);
        drop(other);
        assert_eq!(manager.metrics().expect("released").live_revisions, 0);
    }

    #[test]
    fn handle_exposes_borrowed_access_and_owned_validated_slices() {
        let manager = SnapshotManager::new(quota());
        let handle = manager
            .issue("syntax", DocumentId::new(1), DocumentRevision::new(9), "aβ")
            .expect("snapshot");
        assert_eq!(handle.with_text(str::len), 3);
        assert_eq!(handle.slice(1..3).expect("UTF-8 slice"), "β");
        assert!(matches!(
            handle.slice(1..2),
            Err(SnapshotError::InvalidRange { .. })
        ));
        let metrics = manager.metrics().expect("metrics");
        assert_eq!(metrics.oldest_live_revision, Some(DocumentRevision::new(9)));
        assert_eq!(metrics.retained_snapshot_bytes, 3);
        assert_eq!(metrics.held_too_long.len(), 1);
    }
}

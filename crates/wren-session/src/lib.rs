#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod authority;
mod journal;
mod local;
mod outbox;
mod record;
mod wal;
#[cfg(test)]
mod workspace;

pub use authority::{AuthorityDocument, AuthorityError, MutationSubmission, SessionAuthority};
pub use journal::{SessionJournal, SessionJournalError};
pub use local::{DocumentEncoding, FileIdentity, FileStamp, LineEnding, LocalDocument, OpenedDocument, SaveError, SaveReport, SaveWarning};
pub use outbox::{MutationOutbox, OutboxError};
pub use wal::{LocalWal, RecoveredState, WalError};
#[cfg(test)]
pub use workspace::{PersistBatchReport, PersistBatchState, WorkspaceDocument, WorkspaceError, WorkspaceExecutor};

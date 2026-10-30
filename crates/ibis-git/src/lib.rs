//! ibis-git — credential-injecting git CLI runner and the git plumbing
//! surfaces (anchors, ancestry cache, claim notes/commits, owned-clone sync)
//! consumed by the search, ingest, and server lanes. CLI only, never libgit2
//! (D5).

pub mod ancestry;
pub mod anchor;
pub mod claim;
pub mod error;
pub mod notes;
pub mod resolve;
pub mod runner;
pub mod store;
pub mod sync;
pub mod types;

pub use ancestry::{ancestor_set_for, hold_at, AncestorCache, AncestorSet};
pub use anchor::{GitAnchorRunner, GitAnchorSource};
pub use claim::{GitClaimWriter, GitCommitter};
pub use error::GitError;
pub use notes::{claim_note_message, GitNotesRunner, GitNotesSource, NOTES_REF};
pub use resolve::{detect_provider, get_host, is_auth_error, resolve_credential};
pub use runner::{parse_cat_file_batch, GitRunner};
pub use store::{sanitize_record_key, CredentialDb, CredentialStore};
pub use types::{AuthType, Credential};

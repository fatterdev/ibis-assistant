use std::collections::{HashMap, HashSet};

use async_trait::async_trait;

use crate::error::GitError;
use crate::runner::{parse_cat_file_batch, GitRunner};

/// The well-known git notes ref anchoring historical commits to claim ids
/// (the claim-store addendum). Pointer-only cross-links (`claim=<id>` lines),
/// rebuildable from the claim index — never required for correctness.
pub const NOTES_REF: &str = "refs/notes/threski-claims";

/// Performs git notes operations for a notes ref. Split from
/// [`crate::anchor::GitAnchorSource`] so fakes only implement what they use
///
#[async_trait]
pub trait GitNotesSource: Send + Sync {
    /// Returns annotated object oids → note body (trimmed). A missing notes
    /// ref yields an empty map, not an error.
    async fn notes_list(
        &self,
        repo_path: &str,
        reference: &str,
    ) -> Result<HashMap<String, String>, GitError>;
    /// Force-writes the note on oid.
    async fn notes_add(
        &self,
        repo_path: &str,
        reference: &str,
        oid: &str,
        message: &str,
    ) -> Result<(), GitError>;
    /// Drops the note on oid; removing an absent note is a no-op.
    async fn notes_remove(
        &self,
        repo_path: &str,
        reference: &str,
        oid: &str,
    ) -> Result<(), GitError>;
}

/// Production [`GitNotesSource`] backed by git commands (
/// `search.GitNotesRunner`).
#[derive(Clone)]
pub struct GitNotesRunner {
    runner: GitRunner,
}

impl GitNotesRunner {
    pub fn new(runner: GitRunner) -> Self {
        Self { runner }
    }

    pub fn runner(&self) -> &GitRunner {
        &self.runner
    }
}

#[async_trait]
impl GitNotesSource for GitNotesRunner {
    async fn notes_list(
        &self,
        repo_path: &str,
        reference: &str,
    ) -> Result<HashMap<String, String>, GitError> {
        let mut notes = HashMap::new();
        // Probe the ref first: `notes list` on a missing ref errors, which is
        // the normal empty-store outcome, not a failure.
        if self
            .runner
            .run(repo_path, &["rev-parse", "--verify", "--quiet", reference])
            .await
            .is_err()
        {
            return Ok(notes);
        }
        let ref_arg = format!("--ref={reference}");
        let out = self
            .runner
            .run(repo_path, &["notes", &ref_arg, "list"])
            .await
            .map_err(|e| GitError::context("git notes list", e))?;

        // `git notes list` prints "<note_blob_oid> <annotated_object_oid>".
        // Note bodies are read in ONE cat-file --batch pass instead of one
        // `git notes show` per note.
        let mut refs: Vec<(String, String)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut blobs: Vec<String> = Vec::new();
        for line in String::from_utf8_lossy(&out).lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() != 2 {
                continue;
            }
            refs.push((fields[0].to_string(), fields[1].to_string()));
            if seen.insert(fields[0].to_string()) {
                blobs.push(fields[0].to_string());
            }
        }
        if refs.is_empty() {
            return Ok(notes);
        }
        let stdin: String = blobs.iter().map(|b| format!("{b}\n")).collect();
        let batch = self
            .runner
            .run_with_stdin(repo_path, &stdin, &["cat-file", "--batch"])
            .await
            .map_err(|e| GitError::context("git notes list: read note bodies", e))?;
        let bodies = parse_cat_file_batch(&blobs, &batch);
        for (blob, object) in refs {
            if let Some(body) = bodies.get(&blob) {
                notes.insert(object, body.clone());
            }
        }
        Ok(notes)
    }

    async fn notes_add(
        &self,
        repo_path: &str,
        reference: &str,
        oid: &str,
        message: &str,
    ) -> Result<(), GitError> {
        let ref_arg = format!("--ref={reference}");
        self.runner
            .run(
                repo_path,
                &["notes", &ref_arg, "add", "-f", "-m", message, oid],
            )
            .await
            .map_err(|e| GitError::context(format!("git notes add {oid}"), e))?;
        Ok(())
    }

    async fn notes_remove(
        &self,
        repo_path: &str,
        reference: &str,
        oid: &str,
    ) -> Result<(), GitError> {
        let ref_arg = format!("--ref={reference}");
        match self
            .runner
            .run(repo_path, &["notes", &ref_arg, "remove", oid])
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let lowered = e
                    .command_output()
                    .map(str::to_lowercase)
                    .unwrap_or_default();
                if lowered.contains("no note") || lowered.contains("does not exist") {
                    Ok(())
                } else {
                    Err(GitError::context(format!("git notes remove {oid}"), e))
                }
            }
        }
    }
}

/// Renders the deterministic pointer note for one commit: one `claim=<id>`
/// line per anchored claim, sorted (the claim-store addendum).
pub fn claim_note_message<'a, I>(ids: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let mut lines: Vec<String> = ids.into_iter().map(|id| format!("claim={id}")).collect();
    lines.sort();
    lines.dedup();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_message_sorted_deduped() {
        assert_eq!(claim_note_message(["b", "a", "b"]), "claim=a\nclaim=b");
        assert_eq!(claim_note_message(std::iter::empty::<&str>()), "");
        assert_eq!(claim_note_message(["only"]), "claim=only");
    }

    #[test]
    fn notes_ref_is_stable() {
        assert_eq!(NOTES_REF, "refs/notes/threski-claims");
    }
}

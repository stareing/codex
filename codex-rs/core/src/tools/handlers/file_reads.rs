//! What `Read` has shown the model in this session, per file: lets `Edit`/`Write` refuse to change a file the model
//! has not read or that changed since it read it, and lets a repeated `Read` of an unchanged file answer with a
//! short note instead of the same content again. Cleared whenever the history is replaced (compaction, rollback),
//! since the earlier reads are then no longer in the model's context.

use std::collections::HashMap;
use std::hash::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;

/// Files remembered at most; past this the ledger starts over (the model just reads again).
const MAX_FILES: usize = 2000;

/// (environment id, absolute path)
pub(crate) type FileKey = (String, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mark {
    /// The file's content as the model last saw it (read, or written by `Edit`/`Write`).
    digest: u64,
    /// `(offset, limit)` of the last `Read` that showed this content; `None` after the model's own change.
    view: Option<(usize, usize)>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadOutcome {
    /// Show the content.
    Show,
    /// Same lines of the same content as the last `Read`: it is still in the context.
    Unchanged,
}

#[derive(Debug, Default)]
pub(crate) struct FileReadLedger {
    marks: HashMap<FileKey, Mark>,
}

fn digest(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl FileReadLedger {
    fn put(&mut self, key: FileKey, mark: Mark) {
        if self.marks.len() >= MAX_FILES && !self.marks.contains_key(&key) {
            self.marks.clear();
        }
        self.marks.insert(key, mark);
    }

    /// Records a `Read` of `(offset, limit)` of `text`.
    pub(crate) fn read(
        &mut self,
        key: FileKey,
        text: &str,
        offset: usize,
        limit: usize,
    ) -> ReadOutcome {
        let mark = Mark {
            digest: digest(text),
            view: Some((offset, limit)),
        };
        let outcome = if self.marks.get(&key) == Some(&mark) {
            ReadOutcome::Unchanged
        } else {
            ReadOutcome::Show
        };
        self.put(key, mark);
        outcome
    }

    /// Records the model's own change: it knows the new content, but has not seen it as `Read` output.
    pub(crate) fn wrote(&mut self, key: FileKey, text: &str) {
        self.put(
            key,
            Mark {
                digest: digest(text),
                view: None,
            },
        );
    }

    /// Whether the model may change the file whose current content is `current` (`None`: no such file yet).
    pub(crate) fn may_change(&self, key: &FileKey, current: Option<&str>) -> Result<(), Staleness> {
        let Some(current) = current else {
            return Ok(());
        };
        match self.marks.get(key) {
            None => Err(Staleness::NotRead),
            Some(mark) if mark.digest != digest(current) => Err(Staleness::ChangedSinceRead),
            Some(_) => Ok(()),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.marks.clear();
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Staleness {
    NotRead,
    ChangedSinceRead,
}

impl Staleness {
    pub(crate) fn message(&self, shown: &str) -> String {
        match self {
            Staleness::NotRead => format!(
                "`{shown}` exists and has not been read in this conversation; Read it first"
            ),
            Staleness::ChangedSinceRead => format!(
                "`{shown}` changed since it was last read (by the user, a command, or a formatter); Read it again before changing it"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn key(path: &str) -> FileKey {
        ("local".to_string(), path.to_string())
    }

    #[test]
    fn a_repeated_read_of_the_same_unchanged_lines_is_noted_as_unchanged() {
        let mut ledger = FileReadLedger::default();
        assert_eq!(ledger.read(key("a"), "x\n", 1, 2000), ReadOutcome::Show);
        assert_eq!(
            ledger.read(key("a"), "x\n", 1, 2000),
            ReadOutcome::Unchanged
        );
        assert_eq!(
            ledger.read(key("a"), "x\n", 5, 2000),
            ReadOutcome::Show,
            "other lines"
        );
        assert_eq!(
            ledger.read(key("a"), "y\n", 5, 2000),
            ReadOutcome::Show,
            "changed content"
        );
        assert_eq!(
            ledger.read(key("b"), "y\n", 5, 2000),
            ReadOutcome::Show,
            "other file"
        );
        ledger.wrote(key("a"), "z\n");
        assert_eq!(
            ledger.read(key("a"), "z\n", 5, 2000),
            ReadOutcome::Show,
            "after its own change"
        );
        ledger.clear();
        assert_eq!(
            ledger.read(key("b"), "y\n", 5, 2000),
            ReadOutcome::Show,
            "after the history was replaced"
        );
    }

    #[test]
    fn changing_a_file_needs_a_read_of_its_current_content() {
        let mut ledger = FileReadLedger::default();
        assert_eq!(
            ledger.may_change(&key("new"), None),
            Ok(()),
            "a new file needs no read"
        );
        assert_eq!(
            ledger.may_change(&key("a"), Some("x")),
            Err(Staleness::NotRead)
        );
        ledger.read(key("a"), "x", 1, 10);
        assert_eq!(
            ledger.may_change(&key("a"), Some("x")),
            Ok(()),
            "a partial read is enough"
        );
        assert_eq!(
            ledger.may_change(&key("a"), Some("x2")),
            Err(Staleness::ChangedSinceRead)
        );
        ledger.wrote(key("a"), "x2");
        assert_eq!(
            ledger.may_change(&key("a"), Some("x2")),
            Ok(()),
            "its own change counts as seen"
        );
        ledger.clear();
        assert_eq!(
            ledger.may_change(&key("a"), Some("x2")),
            Err(Staleness::NotRead)
        );
    }
}

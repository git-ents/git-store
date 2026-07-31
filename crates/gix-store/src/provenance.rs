//! [`SchemaLabel`]: the `Schema:` trailer every data commit carries.

use std::fmt;

use facet_git_tree::ObjectId;

use crate::error::Error;

/// Identifies the schema version associated with a stored value.
///
/// The label is metadata for history; reads use the schema bound to the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaLabel(ObjectId);

impl SchemaLabel {
    /// The schema commit id recorded in the trailer.
    pub fn recorded(&self) -> ObjectId {
        self.0
    }
}

impl fmt::Display for SchemaLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Parse the `Schema:` trailer out of a commit message. The *last* `Schema:`
/// line wins, so a caller-supplied message cannot shadow the trailer a write
/// always appends last.
pub(crate) fn parse(commit: ObjectId, message: &[u8]) -> Result<SchemaLabel, Error> {
    let hex = message
        .split(|&b| b == b'\n')
        .filter_map(|line| line.strip_prefix(b"Schema: "))
        .map(<[u8]>::trim_ascii)
        .next_back();
    match hex {
        Some(hex) => ObjectId::from_hex(hex)
            .map(SchemaLabel)
            .map_err(|_| Error::InvalidTrailer {
                commit,
                text: String::from_utf8_lossy(hex).into_owned(),
            }),
        None => Err(Error::MissingTrailer { commit }),
    }
}

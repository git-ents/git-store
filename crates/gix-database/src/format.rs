//! The snapshot tree format: reserved names, metadata encoding, table-name
//! rules, and the ref namespace.

use std::fmt;

use crate::error::{SnapshotError, TableNameError};

/// The first metadata line: format identity and version in one string.
/// The snapshot metadata format line.
///
/// `v2` stores every table value as one self-describing JSON text blob, so
/// scalar types round trip; `v1` leaves encoded through the facet tree
/// codec coerced numbers and booleans to strings and lost `null`.
pub const SNAPSHOT_FORMAT_LINE: &[u8] = b"git-store-database v2";

/// The tree-entry name reserved for a snapshot's metadata blob.
///
/// Table names can never collide with it: they may not start with `!`.
pub const METADATA_NAME: &str = "!database";

/// The ref prefix holding all database state: `refs/db`.
pub const REF_PREFIX: &str = "refs/db";

/// The branch `init` points `HEAD` at.
pub const DEFAULT_BRANCH: &str = "main";

/// The second metadata line's key: pins the snapshot's [git_prolly::ProllyConfig].
pub const PROLLY_CONFIG_LINE: &str = "prolly";

/// Encode [git_prolly::ProllyConfig] as the metadata blob's second line.
#[must_use]
pub fn encode_config(config: git_prolly::ProllyConfig) -> String {
    format!(
        "{PROLLY_CONFIG_LINE} bits={} min_entries={} max_entries={} key_codec={}",
        config.bits,
        config.min_entries,
        config.max_entries,
        match config.key_codec {
            git_prolly::KeyCodecKind::Hex => "hex",
        }
    )
}

/// Parse the metadata blob's second line back into a [git_prolly::ProllyConfig].
///
/// # Errors
///
/// Returns [`SnapshotError::MalformedMetadata`] for any line that is not a
/// product of [`encode_config`].
pub fn parse_config(line: &str) -> Result<git_prolly::ProllyConfig, SnapshotError> {
    let bad = || SnapshotError::MalformedMetadata(line.to_owned());
    let mut fields = line.split_ascii_whitespace();
    if fields.next() != Some(PROLLY_CONFIG_LINE) {
        return Err(bad());
    }
    let mut config = git_prolly::ProllyConfig::default();
    for field in fields {
        let (key, value) = field.split_once('=').ok_or_else(bad)?;
        let number = || value.parse::<usize>().map_err(|_| bad());
        match key {
            "bits" => config.bits = u8::try_from(number()?).map_err(|_| bad())?,
            "min_entries" => config.min_entries = number()?,
            "max_entries" => config.max_entries = number()?,
            "key_codec" if value == "hex" => {}
            _ => return Err(bad()),
        }
    }
    config
        .validate()
        .map_err(|error| SnapshotError::InvalidConfig(error.to_string()))?;
    Ok(config)
}

/// The name of a database table, validated at construction.
///
/// Rules: valid UTF-8, 1..=255 bytes, no `/`, no ASCII whitespace, never
/// `.`, `..`, or anything starting with `!`. These are exactly the names a
/// Git tree entry can carry that stay unambiguous in the snapshot layout and
/// in CLI output.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TableName(String);

impl TableName {
    /// Validate and construct a table name.
    ///
    /// # Errors
    ///
    /// Returns [`TableNameError`] for any name the snapshot format cannot
    /// represent.
    pub fn new(name: &str) -> Result<Self, TableNameError> {
        validate(name)?;
        Ok(Self(name.to_owned()))
    }

    /// The validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TableName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::borrow::Borrow<str> for TableName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for TableName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

fn validate(name: &str) -> Result<(), TableNameError> {
    if name.is_empty() {
        return Err(TableNameError::Empty);
    }
    if name.len() > 255 {
        return Err(TableNameError::TooLong(name.len()));
    }
    if name.starts_with('!') {
        return Err(TableNameError::Reserved(name.to_owned()));
    }
    if name == "." || name == ".." {
        return Err(TableNameError::Reserved(name.to_owned()));
    }
    if name.contains('/') {
        return Err(TableNameError::Slash(name.to_owned()));
    }
    if name.chars().any(|c: char| c.is_ascii_whitespace()) {
        return Err(TableNameError::Whitespace(name.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata_bytes;
    use git_prolly::ProllyConfig;

    #[test]
    fn config_line_round_trips() {
        let line = encode_config(ProllyConfig::default());
        println!("LINE = {line:?}");
        let parsed = parse_config(&line).expect("parse");
        assert_eq!(parsed, ProllyConfig::default());
    }

    #[test]
    fn metadata_bytes_shape() {
        let bytes = metadata_bytes(ProllyConfig::default());
        println!("BYTES = {:?}", String::from_utf8_lossy(&bytes));
        let text = std::str::from_utf8(&bytes).unwrap();
        let mut lines = text.lines();
        let first = lines.next().unwrap();
        println!("FIRST = {first:?}");
        assert_eq!(first.as_bytes(), SNAPSHOT_FORMAT_LINE);
        parse_config(lines.next().unwrap()).expect("config line parses");
    }
}

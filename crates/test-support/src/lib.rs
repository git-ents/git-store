//! Shared scaffolding for the workspace's integration tests.

use std::io::Write;
use std::path::Path;

/// Initialize a git repository at `path` with a committer identity configured,
/// so `gix::Repository::commit` has an author and committer to record.
pub fn init_repo(path: &Path) {
    let repo = gix::init(path).expect("init repo");
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(repo.git_dir().join("config"))
        .expect("open config");
    writeln!(config, "[user]\n\tname = Test\n\temail = test@example.com").expect("write config");
}

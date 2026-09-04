//! Shared helpers for the git-prolly integration test suite.

#![expect(dead_code, reason = "not every test suite uses every helper")]

use std::path::Path;

use facet_value::{VObject, Value};
use tempfile::TempDir;

/// A temporary repository plus an open handle to it. The temporary directory
/// lives as long as the `TestRepo`; bind the whole struct, not just `repo`.
pub struct TestRepo {
    pub _dir: TempDir,
    pub repo: gix::Repository,
}

/// Create a fresh temporary repository.
pub fn repo() -> TestRepo {
    let dir = TempDir::new().expect("create temp dir");
    let repo = gix::init(dir.path()).expect("init repository");
    TestRepo { _dir: dir, repo }
}

/// Build a map value: an object of string fields.
pub fn user(name: &str, email: &str) -> Value {
    let mut object = VObject::new();
    object.insert("name", Value::from(name));
    object.insert("email", Value::from(email));
    Value::from(object)
}

/// A deterministic pseudo-random generator (splitmix64) for test data.
pub struct Rng(u64);

impl Rng {
    /// Seed the generator.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next pseudo-random u64.
    pub fn next_u64(&mut self) -> u64 {
        Self::next(&mut self.0)
    }

    /// A random key of `len` bytes.
    pub fn key(&mut self, len: usize) -> Vec<u8> {
        (0..len)
            .map(|_| (Self::next(&mut self.0) & 0xff) as u8)
            .collect()
    }

    /// The next pseudo-random u64 from a raw state word.
    fn next(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Run `git` with the given arguments inside `dir`, asserting success.
pub fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

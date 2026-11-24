//! Lets jobs inherit the agent's `ACTIONS_RESULTS_URL` instead of the runner's per-job value.
//!
//! The official runner overwrites `ACTIONS_RESULTS_URL` in every step's environment with GitHub's
//! results service. Renaming that UTF-16 string literal inside `Runner.Worker.dll` to
//! `ACTIONS_RESULTS_ORL` (same length) makes the runner export GitHub's URL under the unused name,
//! so the value the agent sets on the runner process reaches `actions/cache` unchanged.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const ORIGINAL: &str = "ACTIONS_RESULTS_URL";
const REDIRECTED: &str = "ACTIONS_RESULTS_ORL";

#[derive(Debug, PartialEq, Eq)]
pub enum Redirect {
    Applied { replacements: usize },
    AlreadyApplied,
}

pub fn worker_assembly(runner_dir: &Path) -> PathBuf {
    runner_dir.join("bin").join("Runner.Worker.dll")
}

/// Rewrites the runner worker assembly in place (atomic rename, permissions preserved).
pub fn apply(assembly: &Path) -> io::Result<Redirect> {
    let original = utf16le(ORIGINAL);
    let redirected = utf16le(REDIRECTED);
    let mut bytes = fs::read(assembly)?;
    let replacements = replace_all(&mut bytes, &original, &redirected);
    if replacements == 0 {
        return if contains(&bytes, &redirected) {
            Ok(Redirect::AlreadyApplied)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runner worker assembly does not reference ACTIONS_RESULTS_URL; unsupported runner version",
            ))
        };
    }
    let permissions = fs::metadata(assembly)?.permissions();
    let staged = assembly.with_extension("dll.open-ci-tmp");
    fs::write(&staged, &bytes)?;
    fs::set_permissions(&staged, permissions)?;
    fs::rename(&staged, assembly)?;
    Ok(Redirect::Applied { replacements })
}

fn utf16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn replace_all(haystack: &mut [u8], from: &[u8], to: &[u8]) -> usize {
    let mut count = 0;
    let mut start = 0;
    while let Some(offset) = haystack[start..]
        .windows(from.len())
        .position(|window| window == from)
    {
        let at = start + offset;
        haystack[at..at + to.len()].copy_from_slice(to);
        start = at + to.len();
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assembly_with(literals: &[&str]) -> io::Result<(tempfile::TempDir, PathBuf)> {
        let dir = tempfile::TempDir::new()?;
        let path = worker_assembly(dir.path());
        fs::create_dir_all(path.parent().unwrap_or(dir.path()))?;
        let mut bytes = b"MZ\x90\x00header".to_vec();
        for literal in literals {
            bytes.extend(utf16le(literal));
            bytes.extend([0u8; 7]);
        }
        fs::write(&path, bytes)?;
        Ok((dir, path))
    }

    #[test]
    fn renames_every_literal_and_is_idempotent() -> io::Result<()> {
        let (_dir, path) = assembly_with(&[ORIGINAL, "ACTIONS_RUNTIME_TOKEN", ORIGINAL])?;
        let size = fs::metadata(&path)?.len();
        assert_eq!(apply(&path)?, Redirect::Applied { replacements: 2 });
        let patched = fs::read(&path)?;
        assert_eq!(
            patched.len() as u64,
            size,
            "same-length rewrite keeps offsets valid"
        );
        assert!(!contains(&patched, &utf16le(ORIGINAL)));
        assert!(contains(&patched, &utf16le("ACTIONS_RUNTIME_TOKEN")));
        assert_eq!(apply(&path)?, Redirect::AlreadyApplied);
        Ok(())
    }

    #[test]
    fn unknown_runner_build_is_rejected_untouched() -> io::Result<()> {
        let (_dir, path) = assembly_with(&["SOMETHING_ELSE"])?;
        let before = fs::read(&path)?;
        assert!(apply(&path).is_err());
        assert_eq!(fs::read(&path)?, before);
        assert!(apply(&path.with_file_name("missing.dll")).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn preserves_permissions() -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = assembly_with(&[ORIGINAL])?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o744))?;
        apply(&path)?;
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o744);
        Ok(())
    }
}

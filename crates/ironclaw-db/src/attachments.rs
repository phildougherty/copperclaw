//! Safety-checked attachment file IO.
//!
//! Inbound and outbound attachment files live under
//! `<session>/inbox/<msg_id>/<filename>` and
//! `<session>/outbox/<msg_id>/<filename>`. Filenames originate from
//! untrusted senders, so we centralize the safety checks here.

use crate::DbError;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const MAX_FILENAME_LEN: usize = 255;

/// Returns `Ok(())` if the candidate name is safe to write as a single
/// path component. Rejects path separators, `..`, leading dots, NUL,
/// control characters, and oversized names.
pub fn safe_attachment_name(name: &str) -> Result<(), DbError> {
    if name.is_empty() {
        return Err(DbError::invariant("filename is empty"));
    }
    if name.len() > MAX_FILENAME_LEN {
        return Err(DbError::invariant("filename too long"));
    }
    if name.starts_with('.') {
        return Err(DbError::invariant("filename starts with '.'"));
    }
    if name == "." || name == ".." {
        return Err(DbError::invariant("filename is . or .."));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(DbError::invariant("filename has path separator or NUL"));
    }
    if name.chars().any(char::is_control) {
        return Err(DbError::invariant("filename has control character"));
    }
    Ok(())
}

fn ensure_dir(p: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(p)
}

/// Extract `bytes` into `<inbox_root>/<msg_id>/<filename>`. Uses
/// `O_EXCL | O_NOFOLLOW` so an attacker cannot pre-place a symlink to
/// trick us into writing somewhere else. Verifies that the resulting
/// canonical path lies inside `inbox_root`.
pub fn extract_to_inbox(
    inbox_root: &Path,
    msg_id: &str,
    filename: &str,
    bytes: &[u8],
) -> Result<PathBuf, DbError> {
    safe_attachment_name(filename)?;
    safe_attachment_name(msg_id)?;

    let msg_dir = inbox_root.join(msg_id);
    ensure_dir(&msg_dir)?;
    refuse_symlink_at(&msg_dir)?;

    let target = msg_dir.join(filename);
    let mut opts = OpenOptions::new();
    opts.create_new(true).write(true);
    opts.custom_flags(libc::O_NOFOLLOW);
    let mut file = opts.open(&target)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    let real_inbox = canonical_existing(inbox_root)?;
    let real_target = canonical_existing(&target)?;
    if !real_target.starts_with(&real_inbox) {
        let _ = std::fs::remove_file(&target);
        return Err(DbError::invariant("extracted path escaped inbox root"));
    }
    Ok(target)
}

/// Read `<outbox_root>/<msg_id>/<filename>` with the same safety checks.
pub fn read_from_outbox(
    outbox_root: &Path,
    msg_id: &str,
    filename: &str,
) -> Result<Vec<u8>, DbError> {
    safe_attachment_name(filename)?;
    safe_attachment_name(msg_id)?;

    let path = outbox_root.join(msg_id).join(filename);
    refuse_symlink_at(&path)?;

    let real_outbox = canonical_existing(outbox_root)?;
    let real_target = canonical_existing(&path)?;
    if !real_target.starts_with(&real_outbox) {
        return Err(DbError::invariant("path escaped outbox root"));
    }

    // O_NOFOLLOW on the final component (defence in depth — already
    // validated above via symlink_metadata).
    let mut opts = OpenOptions::new();
    opts.read(true);
    opts.custom_flags(libc::O_NOFOLLOW);
    let mut file = opts.open(&path)?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut buf)?;
    Ok(buf)
}

fn refuse_symlink_at(path: &Path) -> Result<(), DbError> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(DbError::invariant("symlink rejected"));
        }
    }
    Ok(())
}

fn canonical_existing(path: &Path) -> Result<PathBuf, DbError> {
    std::fs::canonicalize(path).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn safe_name_accepts_normal() {
        for n in ["file.txt", "image.png", "data_01.csv", "RFC-2119.pdf"] {
            safe_attachment_name(n).unwrap();
        }
    }

    #[test]
    fn safe_name_rejects_dangerous() {
        for n in [
            "",
            ".",
            "..",
            ".hidden",
            "../escape",
            "sub/dir",
            "back\\slash",
            "null\0byte",
            "ctrl\x07",
        ] {
            assert!(safe_attachment_name(n).is_err(), "should reject `{n}`");
        }
        let long = "a".repeat(MAX_FILENAME_LEN + 1);
        assert!(safe_attachment_name(&long).is_err());
    }

    #[test]
    fn extract_writes_and_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();

        let path = extract_to_inbox(&inbox, "msg_abc", "hello.txt", b"hi").unwrap();
        assert!(path.starts_with(&inbox));
        let content = std::fs::read(&path).unwrap();
        assert_eq!(content, b"hi");
    }

    #[test]
    fn extract_refuses_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();

        extract_to_inbox(&inbox, "m1", "a.txt", b"1").unwrap();
        let err = extract_to_inbox(&inbox, "m1", "a.txt", b"2").unwrap_err();
        assert!(matches!(err, DbError::Io(_)));
    }

    #[test]
    fn extract_refuses_symlink_pre_placed_at_target() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(inbox.join("m1")).unwrap();

        symlink(other.join("evil.txt"), inbox.join("m1").join("a.txt")).unwrap();

        let err = extract_to_inbox(&inbox, "m1", "a.txt", b"x").unwrap_err();
        // O_NOFOLLOW makes open fail with ELOOP.
        assert!(matches!(err, DbError::Io(_)));
    }

    #[test]
    fn extract_rejects_bad_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let err = extract_to_inbox(&inbox, "m1", "../escape", b"x").unwrap_err();
        assert!(matches!(err, DbError::Invariant(_)));
    }

    #[test]
    fn read_from_outbox_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = tmp.path().join("outbox");
        let dir = outbox.join("m1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"data").unwrap();

        let bytes = read_from_outbox(&outbox, "m1", "a.txt").unwrap();
        assert_eq!(bytes, b"data");
    }

    #[test]
    fn read_from_outbox_refuses_symlink_final_component() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = tmp.path().join("outbox");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(outbox.join("m1")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("real.txt"), b"leak").unwrap();
        symlink(other.join("real.txt"), outbox.join("m1").join("a.txt")).unwrap();

        let err = read_from_outbox(&outbox, "m1", "a.txt").unwrap_err();
        assert!(matches!(err, DbError::Invariant(_)));
    }
}

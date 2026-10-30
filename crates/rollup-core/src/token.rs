//! Local write token. The file is the database path with `.token` appended
//! (`rollup.db` -> `rollup.db.token`), not a replaced extension.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// `db_path` with `.token` appended.
pub fn path_for(db_path: &Path) -> PathBuf {
    let mut owned = db_path.as_os_str().to_owned();
    owned.push(".token");
    PathBuf::from(owned)
}

/// Read the token, or mint it: 32 bytes from `getrandom`, lowercase hex, mode 0600.
///
/// The mint is `create_new`, so two callers cannot truncate each other's file.
/// A symlink, or a file with other hard links, at the token path is refused and
/// left in place. A file that already exists is adopted when its trimmed
/// contents are 64 lowercase hex characters. Anything else is replaced by an
/// atomic rename of a new mint, and the token then on disk is returned.
pub fn ensure(db_path: &Path) -> std::io::Result<String> {
    let path = path_for(db_path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // ponytail: no lock. rename is atomic; the last writer wins and every caller returns what is on disk.
    ensure_at(&path)
}

fn ensure_at(path: &Path) -> std::io::Result<String> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => {
            // umask masks mode() on open: fchmod the handle, not the path, before the secret lands.
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            let token = random_hex()?;
            file.write_all(token.as_bytes())?;
            Ok(token)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let (file, raw) = open_existing(path)?;
            let token = raw.trim();
            if is_token(token) {
                return adopt(path, &file, token);
            }
            // Re-check just before replacing: another process may have minted meanwhile.
            let (file, again) = open_existing(path)?;
            let minted = again.trim();
            if is_token(minted) {
                return adopt(path, &file, minted);
            }
            replace_bad(path)
        }
        Err(e) => Err(e),
    }
}

// Same directory so rename is atomic. A failed mint removes its temp; the caller re-reads the path.
fn replace_bad(path: &Path) -> std::io::Result<String> {
    let mut name = path.as_os_str().to_owned();
    let suffix = random_hex()?;
    name.push(".tmp-");
    name.push(&suffix[..8]);
    let temp = PathBuf::from(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    let minted = (|| -> std::io::Result<()> {
        // umask masks mode() on open: fchmod the handle, not the path, before the secret lands.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        let token = random_hex()?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if let Err(err) = minted {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    let (file, raw) = open_existing(path)?;
    let token = raw.trim();
    if is_token(token) {
        return adopt(path, &file, token);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "write token must be 64 lowercase hex characters",
    ))
}

// O_NOFOLLOW makes the symlink check and the open one step; fchmod acts on the opened file only.
fn open_existing(path: &Path) -> std::io::Result<(File, String)> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("refusing the token file at {}: it is a symlink", path.display()),
            ));
        }
        Err(err) => return Err(err),
    };
    // O_NONBLOCK keeps a planted FIFO from hanging the open; only a regular file is read, and at most 4 KB.
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("refusing the token file at {}: not a regular file", path.display()),
        ));
    }
    // Another hard link is a second route to this inode; refuse before any read, adopt or chmod.
    if meta.nlink() > 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("refusing the token file at {}: it has other hard links", path.display()),
        ));
    }
    // Non-UTF-8 is bad content, so ensure replaces it instead of failing startup.
    let mut bytes = Vec::new();
    (&file).take(4096).read_to_end(&mut bytes)?;
    Ok((file, String::from_utf8(bytes).unwrap_or_default()))
}

// An adopted file keeps the 0600 promise too, whoever created it.
fn adopt(path: &Path, file: &File, token: &str) -> std::io::Result<String> {
    let mode = file.metadata()?.permissions().mode();
    if mode & 0o7777 != 0o600 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(|err| {
            std::io::Error::new(
                err.kind(),
                format!("cannot tighten token file permissions at {}: {err}", path.display()),
            )
        })?;
    }
    Ok(token.to_string())
}

fn is_token(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn random_hex() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)?;
    let mut token = String::with_capacity(64);
    for byte in bytes {
        token.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        token.push(char::from(b"0123456789abcdef"[(byte & 0xf) as usize]));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Tmp(PathBuf);

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch() -> (Tmp, PathBuf) {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "s0tok_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("rollup.db");
        (Tmp(dir), db)
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    // On the open handle, not the path, like the code under test.
    fn set_mode(path: &Path, bits: u32) {
        let file = File::open(path).unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(bits)).unwrap();
    }

    #[test]
    fn ensure_twice_returns_the_same_token_and_keeps_mode_0600() {
        let (_tmp, db) = scratch();
        let first = ensure(&db).unwrap();
        let path = path_for(&db);
        assert!(is_token(&first));
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), first);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(ensure(&db).unwrap(), first);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn a_valid_token_with_a_trailing_newline_is_kept_as_written() {
        let (_tmp, db) = scratch();
        let path = path_for(&db);
        let known = "ab".repeat(32);
        std::fs::write(&path, format!("{known}\n")).unwrap();
        assert_eq!(ensure(&db).unwrap(), known);
        // Adopted, not truncated and rewritten.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{known}\n"));
        assert_eq!(ensure(&db).unwrap(), known);
    }

    #[test]
    fn an_existing_valid_token_file_with_mode_0644_is_adopted_and_ends_at_0600() {
        let (_tmp, db) = scratch();
        let path = path_for(&db);
        let known = "cd".repeat(32);
        std::fs::write(&path, &known).unwrap();
        set_mode(&path, 0o644);
        assert_eq!(mode(&path), 0o644);
        assert_eq!(ensure(&db).unwrap(), known);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), known);
    }

    #[test]
    fn a_file_with_bad_content_is_replaced_with_a_valid_token() {
        let (_tmp, db) = scratch();
        let path = path_for(&db);
        for bad in ["", "nope", "AA", &"A".repeat(64), &"g".repeat(64)] {
            std::fs::write(&path, bad).unwrap();
            let token = ensure(&db).unwrap();
            assert!(is_token(&token), "{bad:?} -> {token}");
            assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), token);
            assert_eq!(mode(&path), 0o600);
            assert_eq!(ensure(&db).unwrap(), token);
        }
    }

    #[test]
    fn a_non_utf8_token_file_is_replaced_with_a_valid_token() {
        let (_tmp, db) = scratch();
        let path = path_for(&db);
        std::fs::write(&path, [0xff, 0xfe, 0x00, b'a']).unwrap();
        let token = ensure(&db).unwrap();
        assert!(is_token(&token));
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), token);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn replacing_a_bad_file_leaves_no_temp_file_behind() {
        let (_tmp, db) = scratch();
        let path = path_for(&db);
        std::fs::write(&path, "not a token").unwrap();
        ensure(&db).unwrap();
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|ent| ent.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![path.file_name().unwrap().to_os_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_valid_0600_token_is_refused_and_the_target_is_unchanged() {
        let (_tmp, db) = scratch();
        let path = path_for(&db);
        let target = path.with_file_name("real.token");
        let known = "ab".repeat(32);
        std::fs::write(&target, &known).unwrap();
        set_mode(&target, 0o600);
        assert_eq!(mode(&target), 0o600);
        std::os::unix::fs::symlink(&target, &path).unwrap();

        let err = ensure(&db).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            err.to_string(),
            format!("refusing the token file at {}: it is a symlink", path.display())
        );
        assert_eq!(std::fs::read_link(&path).unwrap(), target);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), known);
        assert_eq!(mode(&target), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_at_the_token_path_is_refused_without_blocking() {
        let (_tmp, db) = scratch();
        let path = path_for(&db);
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: c is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let err = ensure(&db).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            err.to_string(),
            format!("refusing the token file at {}: not a regular file", path.display())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_at_the_token_path_is_refused_and_not_replaced() {
        let (_tmp, db) = scratch();
        let path = path_for(&db);
        let missing = path.with_file_name("missing.token");
        std::os::unix::fs::symlink(&missing, &path).unwrap();

        let err = ensure(&db).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            err.to_string(),
            format!("refusing the token file at {}: it is a symlink", path.display())
        );
        assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(&path).unwrap(), missing);
        assert!(std::fs::metadata(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_link_to_a_valid_0600_token_is_refused_and_the_original_is_unchanged() {
        let (_tmp, db) = scratch();
        let path = path_for(&db);
        // The original is a real mint (valid token, mode 0600) under another name.
        let other = db.with_file_name("other.db");
        let known = ensure(&other).unwrap();
        let original = path_for(&other);
        assert_eq!(mode(&original), 0o600);
        std::fs::hard_link(&original, &path).unwrap();

        let err = ensure(&db).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            err.to_string(),
            format!("refusing the token file at {}: it has other hard links", path.display())
        );
        // Neither name was read, adopted, chmodded, replaced or deleted.
        assert_eq!(std::fs::read_to_string(&original).unwrap(), known);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), known);
        assert_eq!(mode(&original), 0o600);
        assert_eq!(std::fs::metadata(&original).unwrap().nlink(), 2);
    }
}

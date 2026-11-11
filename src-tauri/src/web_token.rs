//! The web UI's access token: one file, `data_dir/web-token`, that patchbayd and the desktop
//! app both create and read (pb-20261110-04), so a local observer has one discovery path
//! whichever process is running on the machine. Readable by this user only (0600 on unix).

use std::path::Path;

/// The web UI's access token, created on first use (readable by this user only).
/// `rotate` replaces it, signing out every browser and tab.
///
/// Creation is atomic (`create_new`, i.e. `O_CREAT|O_EXCL`): two callers racing on an empty
/// folder end with the winner's one token instead of two interleaved writes, and the file is
/// 0600 (re-asserted whenever this function touches the file, so a pre-existing 0644 cannot
/// persist). The mode is set at creation, never widened.
pub fn load_token(data_dir: &Path, rotate: bool) -> anyhow::Result<String> {
    let path = data_dir.join("web-token");
    if !rotate
        && let Ok(t) = std::fs::read_to_string(&path)
        && !t.trim().is_empty()
    {
        let t = t.trim().to_owned();
        #[cfg(unix)]
        let _ = set_private(&path);
        return Ok(t);
    }
    std::fs::create_dir_all(data_dir)?;
    let token = hex::encode(rand::random::<[u8; 24]>());
    if rotate {
        // A replace must be atomic too: fresh 0600 file beside the target, then rename.
        replace(&path, data_dir, &token)?;
        return Ok(token);
    }
    match create_new(&path, &token) {
        Ok(()) => Ok(token),
        // Another caller won the race: their token is the token — never a second value.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if let Some(t) = poll_written(&path) {
                #[cfg(unix)]
                let _ = set_private(&path);
                return Ok(t);
            }
            // Exists but never became a complete token (a writer died mid-create):
            // recover with an atomic replace rather than leave a broken file behind.
            replace(&path, data_dir, &token)?;
            Ok(token)
        }
        Err(e) => Err(e.into()),
    }
}

/// Create-exclusive: fails with `AlreadyExists` instead of truncating a file another caller
/// just created. 0600 from the moment of creation (unix).
fn create_new(path: &Path, token: &str) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(path)?;
    std::io::Write::write_all(&mut f, token.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Re-assert after the write: creation mode already says 0600; this makes it
        // impossible for the written file to end up wider than 0600.
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Write `token` to a fresh 0600 file beside `path`, then rename it over `path`: readers
/// see either the old file or the new one, never a half-written token.
fn replace(path: &Path, data_dir: &Path, token: &str) -> std::io::Result<()> {
    let tmp = data_dir.join(format!(
        "web-token.{}.{}.tmp",
        std::process::id(),
        hex::encode(rand::random::<[u8; 4]>())
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(&tmp)?;
    std::io::Write::write_all(&mut f, token.as_bytes())?;
    drop(f);
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Bounded wait for a racing creator to finish writing: between `create_new` and the
/// winner's `write_all` the file exists but is empty (or mid-write). Accept a non-empty
/// value that is stable across two reads — 100 x 5ms.
fn poll_written(path: &Path) -> Option<String> {
    let mut prev = String::new();
    for _ in 0..100 {
        if let Ok(t) = std::fs::read_to_string(path) {
            let t = t.trim().to_owned();
            if !t.is_empty() && t == prev {
                return Some(t);
            }
            prev = t;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    None
}

/// Best-effort re-assert of 0600 on an existing file (unix only).
#[cfg(unix)]
fn set_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_processes_read_the_same_token_file() {
        let dir = crate::test_support::short_dir("web-token");
        // The app's share server and patchbayd's web server each call this; the second caller
        // must get the first caller's token, or a consumer could never discover one token.
        let first = load_token(&dir, false).expect("created");
        let second = load_token(&dir, false).expect("read");
        assert_eq!(first, second, "one file, one token for every surface");
        assert_eq!(first.len(), 48, "24 random bytes as hex");
        assert!(first.bytes().all(|c| c.is_ascii_hexdigit()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_callers_racing_on_an_empty_folder_end_with_one_token() {
        // Ship's bounce 2: create_new + AlreadyExists re-read. Both threads start on an
        // empty folder behind a barrier; whichever loses the create must adopt the
        // winner's token, so both callers end with the SAME token.
        let dir = crate::test_support::short_dir("web-token-race");
        std::fs::create_dir_all(&dir).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let d = dir.clone();
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    load_token(&d, false).expect("load")
                })
            })
            .collect();
        let tokens: Vec<String> = handles.into_iter().map(|h| h.join().expect("thread")).collect();
        assert_eq!(tokens[0], tokens[1], "both callers must end with the SAME token");
        let on_disk = std::fs::read_to_string(dir.join("web-token")).unwrap();
        assert_eq!(on_disk.trim(), tokens[0], "the file holds that same token");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_private_to_this_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::short_dir("web-token-mode");
        let token = load_token(&dir, false).unwrap();
        let meta = std::fs::metadata(dir.join("web-token")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600, "readable by this user only");
        // A rotate replaces the value but keeps the file private.
        let rotated = load_token(&dir, true).unwrap();
        assert_ne!(rotated, token, "--rotate issues a new token");
        let mode = std::fs::metadata(dir.join("web-token")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "still 0600 after a rotate");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_pre_existing_0644_does_not_persist() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::short_dir("web-token-wide");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("web-token");
        let existing = "0123456789abcdef0123456789abcdef0123456789abcdef";
        std::fs::write(&path, existing).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        // The existing token is respected (read, not replaced) but the mode is re-asserted.
        let t = load_token(&dir, false).unwrap();
        assert_eq!(t, existing, "a readable existing token is kept");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a pre-existing 0644 must not persist");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

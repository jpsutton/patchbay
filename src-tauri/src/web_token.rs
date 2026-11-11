//! The web UI's access token: one file, `data_dir/web-token`, that patchbayd and the desktop
//! app both create and read (pb-20261110-04), so a local observer has one discovery path
//! whichever process is running on the machine. Readable by this user only (0600 on unix).

use std::path::Path;

/// The web UI's access token, created on first use (readable by this user only).
/// `rotate` replaces it, signing out every browser and tab.
pub fn load_token(data_dir: &Path, rotate: bool) -> anyhow::Result<String> {
    let path = data_dir.join("web-token");
    if !rotate
        && let Ok(t) = std::fs::read_to_string(&path)
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_owned());
    }
    std::fs::create_dir_all(data_dir)?;
    let token = hex::encode(rand::random::<[u8; 24]>());
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(&path)?, token.as_bytes())?;
    Ok(token)
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
}

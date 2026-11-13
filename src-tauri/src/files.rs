//! Project file listing for @-mentions in the Code composer, and the pictures an agent's
//! reply points at. A phone cannot read the computer's files, so the host sends the bytes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

pub const MAX_FILES: usize = 20_000;

/// Files under `root`, relative and `/`-separated, honoring .gitignore and skipping hidden
/// entries and `.git`. Stops after `MAX_FILES`.
pub fn list(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
            if out.len() >= MAX_FILES {
                break;
            }
        }
    }
    out.sort();
    Ok(out)
}

const PASS_THROUGH: u64 = 1_500_000;
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// A data URL for a picture a reply linked. `raw` is a markdown image source: a path relative to
/// `cwd`, an absolute path, or a `file:` URL. The agent can point at a file outside the session
/// folder (a screenshot in another checkout), so the path is not confined to `cwd`. It still has
/// to be an image file on this computer, and a web address is refused.
pub fn session_image(cwd: &Path, raw: &str) -> Result<String> {
    session_image_with(cwd, raw, &patchbay_agent::OsLinks)
}

/// [`session_image`] with the link reader handed in, so a test can see that the filesystem is not
/// asked about a source on another machine's share.
pub fn session_image_with(cwd: &Path, raw: &str, links: &dyn patchbay_agent::LinkReader) -> Result<String> {
    if raw.trim().is_empty() || raw.len() > 4096 {
        bail!("bad image path");
    }
    let scheme = raw.trim().split(':').next().unwrap_or("").to_ascii_lowercase();
    if matches!(scheme.as_str(), "http" | "https" | "data" | "blob") {
        bail!("not a local image");
    }
    let wanted = local_path(raw);
    let joined = {
        let path = PathBuf::from(&wanted);
        if path.is_absolute() { path } else { cwd.join(path) }
    };
    // BEFORE anything is statted or canonicalized: this runs for every picture any reply links, with no
    // click, so another machine's share named here (or a link in the project that points at one) would
    // be connected to, and the user's credentials offered, just by drawing the reply. The lexical check
    // comes first inside `foreign_target_with` and asks the filesystem nothing for a path written as a share.
    if patchbay_agent::foreign_target_with(cwd, &joined, links) {
        bail!("the image is on another machine's share or a device path, or behind a link to one");
    }
    let full = joined.canonicalize().context("no such image")?;
    let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        other => bail!("not an image ({other})"),
    };
    let len = std::fs::metadata(&full)?.len();
    if len > MAX_IMAGE_BYTES {
        bail!("image is too large");
    }
    let bytes = std::fs::read(&full)?;
    if ext != "bmp" && len <= PASS_THROUGH && magic_ok(&bytes, &ext) {
        return Ok(format!("data:{mime};base64,{}", B64.encode(bytes)));
    }
    let img = image::load_from_memory(&bytes)?.thumbnail(1600, 1600);
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(format!("data:image/png;base64,{}", B64.encode(out)))
}

fn local_path(raw: &str) -> String {
    let raw = raw.trim().split(['?', '#']).next().unwrap_or("").trim();
    let rest = if let Some(rest) = raw.strip_prefix("file:") {
        rest.strip_prefix("//localhost").unwrap_or(rest).strip_prefix("//").unwrap_or(rest)
    } else {
        raw
    };
    let path = percent_decode(rest);
    // `file:///C:/...` keeps a leading slash in front of the drive.
    if path.len() >= 3 && path.as_bytes()[0] == b'/' && path.as_bytes()[2] == b':' && path.as_bytes()[1].is_ascii_alphabetic() {
        return path[1..].to_owned();
    }
    path
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn magic_ok(bytes: &[u8], ext: &str) -> bool {
    match ext {
        "png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "jpg" | "jpeg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "webp" => bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn lists_files_and_respects_gitignore() {
        let dir = std::env::temp_dir().join(format!("pb-files-{}", crate::db::new_id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join("target/junk"), "").unwrap();
        std::fs::write(dir.join("README.md"), "").unwrap();
        let files = super::list(&dir).unwrap();
        assert_eq!(files, vec!["README.md".to_string(), "src/main.rs".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn png(dir: &std::path::Path, name: &str) {
        let img = image::RgbImage::from_pixel(2, 2, image::Rgb([10, 20, 30]));
        img.save(dir.join(name)).unwrap();
    }

    #[test]
    fn session_image_reads_a_file_inside_the_folder() {
        let dir = std::env::temp_dir().join(format!("pb-img-{}", crate::db::new_id()));
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        png(&dir, "docs/shot.png");
        std::fs::write(dir.join("notes.txt"), "nope").unwrap();
        let rel = super::session_image(&dir, "docs/shot.png").unwrap();
        let abs = super::session_image(&dir, &dir.join("docs/shot.png").to_string_lossy()).unwrap();
        let url = format!("file://{}", dir.join("docs/shot.png").to_string_lossy());
        let from_url = super::session_image(&dir, &url).unwrap();
        assert!(rel.starts_with("data:image/png;base64,"));
        assert_eq!(rel, abs);
        assert_eq!(rel, from_url);
        assert!(super::session_image(&dir, "notes.txt").is_err());
        assert!(super::session_image(&dir, "https://example.com/a.png").is_err());
        assert!(super::session_image(&dir, "../shot.png").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A picture source that names another machine's share (or a device path), in any spelling, is
    /// refused from its TEXT: the filesystem is asked NOTHING, because asking is the connection that offers
    /// the user's credentials, and this runs for every reply that links a picture, with no click. Spellings
    /// that `local_path` turns into a path on this machine (`file://host/...` is a RELATIVE path under the
    /// folder, `file://///host/...` a path on the drive, a `?` ends the path) never leave the machine.
    #[test]
    #[cfg(windows)]
    fn session_image_refuses_a_share_in_every_spelling_without_asking_the_filesystem() {
        use std::cell::RefCell;
        use std::path::{Path, PathBuf};
        struct Spy(RefCell<Vec<PathBuf>>);
        impl patchbay_agent::LinkReader for Spy {
            fn link_target(&self, p: &Path) -> Option<PathBuf> {
                self.0.borrow_mut().push(p.to_path_buf());
                None
            }
        }
        let cwd = Path::new(r"C:\Users\x\proj");
        let started = std::time::Instant::now();
        // Refused by the check itself, before the reader or the filesystem is asked anything.
        for raw in [
            r"\\192.0.2.1\s\a.png",
            "//192.0.2.1/s/a.png",
            r"\\.\pipe\a.png",
            "file:////192.0.2.1/s/a.png",
            "file:\\\\192.0.2.1\\s\\a.png",
        ] {
            let spy = Spy(RefCell::new(Vec::new()));
            let err = super::session_image_with(cwd, raw, &spy).unwrap_err().to_string();
            assert!(err.contains("another machine"), "{raw}: {err}");
            assert!(spy.0.borrow().is_empty(), "{raw}: the filesystem was asked: {:?}", spy.0.borrow());
        }
        // Spelled so that they are NOT a share once `local_path` has read them: they are refused or not
        // found, and everything the walk looked at is a local path.
        for raw in [
            r"\\?\UNC\192.0.2.1\s\a.png",
            r"\??\UNC\192.0.2.1\s\a.png",
            "file://///192.0.2.1/s/a.png",
            r"file:///\\192.0.2.1\s\a.png",
            "file:///%5C%5C192.0.2.1%5Cs%5Ca.png",
            "file:////%5C%5C192.0.2.1%5Cs%5Ca.png",
            "file://192.0.2.1/s/a.png",
            "file:///192.0.2.1/s/a.png",
            "file://localhost//192.0.2.1/s/a.png",
        ] {
            let spy = Spy(RefCell::new(Vec::new()));
            assert!(super::session_image_with(cwd, raw, &spy).is_err(), "{raw}");
            for asked in spy.0.borrow().iter() {
                assert!(!patchbay_agent::foreign_location(cwd, asked), "{raw}: asked about {}", asked.display());
                assert!(asked.to_string_lossy().starts_with("C:"), "{raw}: asked about {}", asked.display());
            }
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "deciding took {:?}: a network attempt", started.elapsed());
    }

    #[test]
    fn session_image_decodes_a_space_and_refuses_a_symlink_out() {
        let dir = std::env::temp_dir().join(format!("pb-img-{}", crate::db::new_id()));
        let outside = std::env::temp_dir().join(format!("pb-img-out-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        png(&dir, "my shot.png");
        png(&outside, "secret.png");
        let spaced = super::session_image(&dir, "my%20shot.png").unwrap();
        assert!(spaced.starts_with("data:image/png;base64,"));
        let elsewhere = super::session_image(&dir, &outside.join("secret.png").to_string_lossy()).unwrap();
        assert!(elsewhere.starts_with("data:image/png;base64,"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("secret.png"), dir.join("link.png")).unwrap();
            assert_eq!(super::session_image(&dir, "link.png").unwrap(), elsewhere);
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }
}

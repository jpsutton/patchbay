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

//! Project file listing for @-mentions in the Code composer.

use std::path::Path;

use anyhow::Result;

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
    }
}

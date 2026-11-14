//! Windows path helpers shared by Teams resolve, Codex F2 `path_key`, and worker git `-c` flags.

use std::path::PathBuf;

/// Drop an accepted Windows `\\?\` / `\\?\UNC\` prefix so git and other tools accept the path.
/// Other `\\?\` namespaces (Volume GUID, HarddiskVolume, bare drive `\\?\C:` without a separator,
/// incomplete UNC, …) are unresolvable → `None`.
pub(crate) fn strip_windows_verbatim(path: PathBuf) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        return strip_windows_verbatim_str(&path.to_string_lossy()).map(PathBuf::from);
    }
    #[cfg(not(windows))]
    {
        Some(path)
    }
}

/// Pure string form of [`strip_windows_verbatim`] (testable on every OS).
pub(crate) fn strip_windows_verbatim_str(s: &str) -> Option<String> {
    let Some(rest) = s.strip_prefix(r"\\?\") else {
        return Some(s.to_owned());
    };
    if let Some(unc) = rest.strip_prefix(r"UNC\") {
        // Require server\share (at least two non-empty components).
        let mut parts = unc.split('\\');
        let server = parts.next().unwrap_or("");
        let share = parts.next().unwrap_or("");
        if server.is_empty() || share.is_empty() {
            return None;
        }
        return Some(format!(r"\\{unc}"));
    }
    // Drive: only `X:\…` or `X:/…` (len >= 3 with separator). Bare `\\?\C:` is refused.
    let b = rest.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        return Some(rest.to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_windows_verbatim_accepts_drive_and_unc_refuses_other_namespaces() {
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\C:\Users\x\proj"),
            Some(r"C:\Users\x\proj".into())
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\c:\temp"),
            Some(r"c:\temp".into())
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\C:/foo"),
            Some(r"C:/foo".into())
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\UNC\server\share\path"),
            Some(r"\\server\share\path".into())
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\UNC\srv\share"),
            Some(r"\\srv\share".into())
        );
        assert_eq!(strip_windows_verbatim_str(r"C:\x"), Some(r"C:\x".into()));
        for bad in [
            r"\\?\C:",
            r"\\?\c:",
            r"\\?\Volume{00000000-0000-0000-0000-000000000000}\x",
            r"\\?\HarddiskVolume1\Windows",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\x",
            r"\\?\foo",
            r"\\?\UNC\serveronly",
            r"\\?\UNC\",
            r"\\?\C",
            r"\\?\C:noslash",
        ] {
            assert_eq!(strip_windows_verbatim_str(bad), None, "must refuse {bad}");
        }
    }
}

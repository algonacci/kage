//! Path normalisation shared by everything that hands a path to another program.

use std::path::{Path, PathBuf};

/// Resolve a path to an absolute one that other programs can actually use.
///
/// `std::fs::canonicalize` on Windows returns an extended-length path (`\\?\C:\...`). That form is
/// correct for the Win32 API but many tools reject it — git fails outright with "Invalid argument"
/// when asked to create a worktree there, which would break every isolated run on Windows. The
/// prefix is stripped so the path stays ordinary, and paths that cannot be resolved are returned
/// unchanged rather than turned into an error.
pub fn canonical(path: &Path) -> PathBuf {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    strip_verbatim(resolved)
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return path;
    }

    let text = path.to_string_lossy();
    let Some(rest) = text.strip_prefix(r"\\?\") else {
        return path;
    };

    // `\\?\UNC\server\share` is the verbatim spelling of `\\server\share`.
    if let Some(unc) = rest.strip_prefix(r"UNC\") {
        return PathBuf::from(format!(r"\\{unc}"));
    }

    PathBuf::from(rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn the_verbatim_prefix_is_removed() {
        let stripped = strip_verbatim(PathBuf::from(r"\\?\C:\Users\dev\project"));

        assert_eq!(stripped, PathBuf::from(r"C:\Users\dev\project"));
    }

    #[test]
    #[cfg(windows)]
    fn a_verbatim_network_path_becomes_an_ordinary_unc_path() {
        let stripped = strip_verbatim(PathBuf::from(r"\\?\UNC\server\share\code"));

        assert_eq!(stripped, PathBuf::from(r"\\server\share\code"));
    }

    #[test]
    fn an_ordinary_path_is_left_alone() {
        let path = if cfg!(windows) {
            PathBuf::from(r"C:\Users\dev")
        } else {
            PathBuf::from("/home/dev")
        };

        assert_eq!(strip_verbatim(path.clone()), path);
    }

    #[test]
    fn an_unresolvable_path_is_returned_as_given() {
        let path = Path::new("./no-such-directory-anywhere/x");

        assert_eq!(canonical(path), path.to_path_buf());
    }

    #[test]
    fn a_real_directory_resolves_without_a_verbatim_prefix() {
        let resolved = canonical(&std::env::temp_dir());

        assert!(resolved.is_absolute());
        assert!(
            !resolved.to_string_lossy().starts_with(r"\\?\"),
            "git and other tools cannot use this form: {}",
            resolved.display()
        );
    }
}

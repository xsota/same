//! Path helpers.

/// Strips the Windows verbatim prefix that `std::fs::canonicalize` adds
/// (`\\?\C:\x` -> `C:\x`, `\\?\UNC\server\share` -> `\\server\share`).
///
/// Windows `CreateProcess` rejects verbatim paths as a working directory
/// (os error 267), so paths that end up as a child process `cwd` must be
/// in their plain form. Paths without the prefix are returned unchanged.
pub fn simplify_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        // Only strip for drive paths like `C:\...`; other verbatim forms
        // (e.g. `\\?\Volume{...}`) have no plain equivalent.
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            rest.to_string()
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::simplify_path;

    #[test]
    fn strips_verbatim_prefixes() {
        assert_eq!(
            simplify_path(r"\\?\UNC\wsl.localhost\Ubuntu\home\x"),
            r"\\wsl.localhost\Ubuntu\home\x"
        );
        assert_eq!(simplify_path(r"\\?\C:\work\repo"), r"C:\work\repo");
        assert_eq!(simplify_path(r"\\?\Volume{abc}\x"), r"\\?\Volume{abc}\x");
        assert_eq!(simplify_path("/home/user/git/same"), "/home/user/git/same");
        assert_eq!(simplify_path(r"C:\work"), r"C:\work");
    }
}

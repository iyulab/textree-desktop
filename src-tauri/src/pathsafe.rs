//! Node names: what a new note or folder may be called. Where a path may lead — inside the vault
//! or not — is decided in one place, `vault_paths`.

/// Windows reserved device names (reserved regardless of extension).
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Whether the string is safe as a new name for a node (note/folder). Rejects path separators,
/// parent references, reserved dot names, Windows reserved device names, the characters Windows
/// does not allow in a name, and a trailing dot or space — which Windows drops without a word, so
/// the name on disk would not be the one asked for. Held on every platform: a folder of notes
/// moves between machines, and a name one system took that another cannot open strands the note.
pub fn is_valid_name(name: &str) -> bool {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with('.') // avoid collision with reserved/hidden names like .textree
        || name.ends_with('.')
        || name.ends_with(' ')
        || name.chars().any(|c| matches!(c, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*') || c.is_control())
    {
        return false;
    }
    // Reserved regardless of extension, so compare the part before the first '.' in uppercase (e.g. both CON and CON.md are rejected).
    let base = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    !WINDOWS_RESERVED.contains(&base.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_name_rejects_dangerous_inputs() {
        // Non-ASCII names are ordinary note titles, not dangerous input. Keep a non-ASCII
        // fixture here: flattening it to ASCII would leave that case untested.
        assert!(is_valid_name("café-notes"));
        assert!(is_valid_name("2026-06-13"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("."));
        assert!(!is_valid_name(".."));
        assert!(!is_valid_name(".textree"));
        assert!(!is_valid_name("a/b"));
        assert!(!is_valid_name("a\\b"));
        assert!(!is_valid_name("a\0b"));
    }

    #[test]
    fn valid_name_rejects_what_windows_cannot_hold_or_silently_changes() {
        for name in [
            "What?",
            "a:b",
            "a*b",
            "a<b",
            "a>b",
            "a|b",
            "say \"hi\"",
            "tab\there",
        ] {
            assert!(!is_valid_name(name), "{name:?} cannot be a name on Windows");
        }
        // Windows drops a trailing dot or space, so the note would land under another name.
        assert!(!is_valid_name("draft."));
        assert!(!is_valid_name("draft "));
        assert!(is_valid_name("draft.md"));
        assert!(is_valid_name("a b"));
    }

    #[test]
    fn valid_name_rejects_windows_reserved_devices() {
        assert!(!is_valid_name("CON"));
        assert!(!is_valid_name("con")); // case-insensitive
        assert!(!is_valid_name("NUL"));
        assert!(!is_valid_name("COM1"));
        assert!(!is_valid_name("LPT9"));
        assert!(!is_valid_name("CON.md")); // regardless of extension
                                           // Normal names that merely contain a reserved name are allowed.
        assert!(is_valid_name("CONTROL"));
        assert!(is_valid_name("réunion-CON"));
    }
}

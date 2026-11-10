//! What a note points at: the files and notes its markdown links to. Code is not read as links.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `[text](target)` or `![alt](target)` — a path, a web address, or an anchor.
    Path(String),
    /// `[[name]]` or `![[name]]` — a name looked up among the folder's files.
    Wiki(String),
}

/// Whether a name or `/`-separated path is a note: it ends in `.md`, in any case.
pub(crate) fn is_note_path(rel: &str) -> bool {
    let bytes = rel.as_bytes();
    bytes.len() >= 3 && bytes[bytes.len() - 3..].eq_ignore_ascii_case(b".md")
}

/// Removes fenced blocks and inline code, keeping line structure.
fn without_code(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut fence: Option<&str> = None;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        match fence {
            Some(f) => {
                if trimmed.starts_with(f) {
                    fence = None;
                }
                out.push('\n');
                continue;
            }
            None if trimmed.starts_with("```") => {
                fence = Some("```");
                out.push('\n');
                continue;
            }
            None if trimmed.starts_with("~~~") => {
                fence = Some("~~~");
                out.push('\n');
                continue;
            }
            None => {}
        }
        let mut in_code = false;
        for ch in line.chars() {
            if ch == '`' {
                in_code = !in_code;
            } else if !in_code {
                out.push(ch);
            }
        }
        out.push('\n');
    }
    out
}

/// The link targets in `markdown`, in order.
pub fn targets(markdown: &str) -> Vec<Target> {
    let text = without_code(markdown);
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' && bytes.get(i + 1) == Some(&b'[') {
            if let Some(end) = text[i + 2..].find("]]") {
                let inner = &text[i + 2..i + 2 + end];
                let name = inner.split(['|', '#']).next().unwrap_or("").trim();
                if !name.is_empty() {
                    found.push(Target::Wiki(name.to_string()));
                }
                i += end + 4;
                continue;
            }
        }
        if bytes[i] == b']' && bytes.get(i + 1) == Some(&b'(') {
            let rest = &text[i + 2..];
            let target = if let Some(stripped) = rest.strip_prefix('<') {
                stripped.find('>').map(|e| &stripped[..e])
            } else {
                rest.find(')')
                    .map(|e| rest[..e].split_whitespace().next().unwrap_or(""))
            };
            if let Some(t) = target.filter(|t| !t.is_empty()) {
                found.push(Target::Path(t.to_string()));
            }
        }
        if text[i..].starts_with("<img") {
            if let Some(src) = text[i..].find("src=\"").map(|s| i + s + 5) {
                if let Some(end) = text[src..].find('"') {
                    found.push(Target::Path(text[src..src + end].to_string()));
                }
            }
        }
        i += text[i..].chars().next().map_or(1, char::len_utf8);
    }
    found
}

fn percent_decoded(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            // `get` yields None when the slice would split a UTF-8 character: not an escape.
            let hex = s
                .get(i + 1..i + 3)
                .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()));
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `target` (a `Path` target found in `note`) as a `/`-separated path inside the notes folder,
/// or `None` for a web address, a bare anchor, or a path that leaves the folder.
pub fn resolve(note: &str, target: &str) -> Option<String> {
    let target = target.split(['#', '?']).next().unwrap_or("");
    // Checked before and after decoding: `%2F%2F` must not slip past the first check.
    if target.is_empty() || is_network_prefix(target) {
        return None;
    }
    let decoded = percent_decoded(target).replace('\\', "/");
    if is_network_prefix(&decoded) {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    if !decoded.starts_with('/') {
        parts.extend(note.split('/'));
        parts.pop(); // the note's own name
    }
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            // A colon starts a scheme (`https:`, `mailto:`) or a drive (`C:`), and Windows forbids
            // `:` in file names, so nothing legitimate is lost. NUL cannot appear in a path at all.
            s if s.contains([':', '\0']) => return None,
            s => parts.push(s),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// A protocol-relative address (`//host/x`) or a UNC path: never a folder path.
fn is_network_prefix(s: &str) -> bool {
    s.starts_with("//") || s.starts_with("\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_images_and_embeds_are_found() {
        let md = "See ![chart](img/chart.png \"Chart\") and [the plan](../plan.md#goals).\n![[photo.jpg]] and [[Other note|shown]]\n<img src=\"raw.png\">";
        assert_eq!(
            targets(md),
            vec![
                Target::Path("img/chart.png".into()),
                Target::Path("../plan.md#goals".into()),
                Target::Wiki("photo.jpg".into()),
                Target::Wiki("Other note".into()),
                Target::Path("raw.png".into()),
            ]
        );
    }

    #[test]
    fn code_is_not_read_as_links() {
        let md =
            "`![x](inline.png)`\n```\n![y](fenced.png)\n```\n~~~md\n[[z]]\n~~~\n![real](real.png)";
        assert_eq!(targets(md), vec![Target::Path("real.png".into())]);
    }

    #[test]
    fn angle_brackets_and_spaces() {
        assert_eq!(
            targets("![a](<my photo.png>)"),
            vec![Target::Path("my photo.png".into())]
        );
        assert_eq!(
            targets("![a](my%20photo.png)"),
            vec![Target::Path("my%20photo.png".into())]
        );
    }

    #[test]
    fn resolving_stays_inside_the_folder() {
        assert_eq!(
            resolve("blog/post.md", "img/a.png").as_deref(),
            Some("blog/img/a.png")
        );
        assert_eq!(
            resolve("blog/post.md", "../shared/a.png").as_deref(),
            Some("shared/a.png")
        );
        assert_eq!(
            resolve("blog/post.md", "/top.png").as_deref(),
            Some("top.png")
        );
        assert_eq!(
            resolve("blog/post.md", "my%20photo.png").as_deref(),
            Some("blog/my photo.png")
        );
        assert_eq!(
            resolve("blog/post.md", "a.png?raw=1#x").as_deref(),
            Some("blog/a.png")
        );
        assert_eq!(resolve("post.md", "../outside.txt"), None);
        assert_eq!(resolve("post.md", "https://example.com/a.png"), None);
        assert_eq!(resolve("post.md", "mailto:a@example.com"), None);
        assert_eq!(resolve("post.md", "#section"), None);
        assert_eq!(resolve("post.md", "C:/Windows/a.png"), None);
    }

    #[test]
    fn web_drive_and_separator_tricks_never_leave_the_folder() {
        assert_eq!(resolve("post.md", "//example.com/a.png"), None);
        assert_eq!(resolve("post.md", "%2F%2Fexample.com/a.png"), None);
        assert_eq!(resolve("post.md", "/C:/Windows/a.png"), None);
        assert_eq!(resolve("post.md", "./C:/x"), None);
        assert_eq!(resolve("post.md", "C%3A/Windows/a.png"), None);
        assert_eq!(resolve("post.md", "..\\..\\secret.txt"), None);
        assert_eq!(resolve("post.md", "..%5Csecret.txt"), None);
        assert_eq!(resolve("post.md", "%2e%2e/x"), None);
        assert_eq!(resolve("post.md", "a%00.png"), None);
        assert_eq!(
            resolve("blog/post.md", "img\\a.png").as_deref(),
            Some("blog/img/a.png")
        );
    }

    #[test]
    fn a_percent_before_a_non_ascii_character_is_not_an_escape() {
        assert_eq!(resolve("post.md", "%é").as_deref(), Some("%é"));
        assert_eq!(resolve("post.md", "%aé").as_deref(), Some("%aé"));
    }
}

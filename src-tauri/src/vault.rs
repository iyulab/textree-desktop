use serde::Serialize;
use std::path::{Path, PathBuf};
use std::{fs, io};

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Leaf,
    Container,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TreeNode {
    /// Display name (without the `.md` extension)
    pub name: String,
    pub kind: NodeKind,
    /// The node's own filesystem path. A leaf is a `.md` file, a container is a directory.
    /// This lets structural edits (rename/move/delete) address containers that have no body too.
    pub path: PathBuf,
    /// Path to the `.md` file holding this node's body. `None` for a container without a body.
    pub body_path: Option<PathBuf>,
    pub children: Vec<TreeNode>,
}

/// Builds the list of top-level nodes at the vault root.
pub fn build_tree(root: &Path) -> io::Result<Vec<TreeNode>> {
    build_children(root, None) // the root has no parent node that could own a folder note
}

fn build_children(dir: &Path, skip_note: Option<&str>) -> io::Result<Vec<TreeNode>> {
    // Per-entry errors (e.g. permission denied on a specific file) are intentionally ignored —
    // if one note is inaccessible we build a partial tree rather than aborting the whole tree.
    let mut entries: Vec<_> = fs::read_dir(dir)?.filter_map(|r| r.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    let mut nodes = Vec::new();
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            nodes.push(build_node_for_dir(&path)?);
        } else if is_markdown(&name) {
            // the folder note (this directory's body) is excluded via the skip_note set by the parent
            if Some(name.as_str()) == skip_note {
                continue;
            }
            nodes.push(TreeNode {
                name: strip_md(&name),
                kind: NodeKind::Leaf,
                path: path.clone(),
                body_path: Some(path),
                children: Vec::new(),
            });
        }
    }
    Ok(nodes)
}

fn build_node_for_dir(dir: &Path) -> io::Result<TreeNode> {
    let name = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let body = dir.join(format!("{name}.md"));
    // A body path that leads out through a link is refused where paths come back in: every note
    // path a command takes is resolved by `vault_paths`.
    let body_path = if body.is_file() { Some(body) } else { None };
    let folder_note = format!("{name}.md");
    let children = build_children(dir, Some(&folder_note))?;
    Ok(TreeNode {
        name,
        kind: NodeKind::Container,
        path: dir.to_path_buf(),
        body_path,
        children,
    })
}

fn is_markdown(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".md")
}

fn strip_md(name: &str) -> String {
    // strip only the trailing ".md" (case-insensitive)
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".md") {
        name[..name.len() - 3].to_string()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn touch(dir: &Path, rel: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, b"x").unwrap();
    }

    #[test]
    fn empty_vault_yields_empty_tree() {
        let tmp = TempDir::new().unwrap();
        let tree = build_tree(tmp.path()).unwrap();
        assert!(tree.is_empty());
    }

    #[test]
    fn leaf_note_is_mapped() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "project.md");
        let tree = build_tree(tmp.path()).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].name, "project");
        assert_eq!(tree[0].kind, NodeKind::Leaf);
        assert_eq!(tree[0].path, tmp.path().join("project.md"));
        assert!(tree[0].body_path.is_some());
        assert!(tree[0].children.is_empty());
    }

    #[test]
    fn container_path_is_directory_even_without_body() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "assets/diagram.png"); // container without a body
        let tree = build_tree(tmp.path()).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].kind, NodeKind::Container);
        assert_eq!(tree[0].path, tmp.path().join("assets"));
        assert!(tree[0].body_path.is_none());
    }

    #[test]
    fn folder_note_becomes_container_body_not_child() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "journal/journal.md"); // folder note (body)
        touch(tmp.path(), "journal/2026-06-13.md"); // child leaf
        let tree = build_tree(tmp.path()).unwrap();
        assert_eq!(tree.len(), 1);
        let diary = &tree[0];
        assert_eq!(diary.name, "journal");
        assert_eq!(diary.kind, NodeKind::Container);
        assert!(
            diary.body_path.is_some(),
            "folder note should be picked up as the body"
        );
        assert_eq!(diary.children.len(), 1);
        assert_eq!(diary.children[0].name, "2026-06-13");
    }

    #[test]
    fn non_markdown_files_are_hidden() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "library/library.md");
        touch(tmp.path(), "library/assets/diagram.png"); // not shown in the tree
        let tree = build_tree(tmp.path()).unwrap();
        let archive = &tree[0];
        assert_eq!(archive.children.len(), 1);
        let assets = &archive.children[0];
        assert_eq!(assets.name, "assets");
        assert_eq!(assets.kind, NodeKind::Container);
        assert!(assets.body_path.is_none());
        assert!(assets.children.is_empty(), "png is excluded from the tree");
    }

    #[test]
    fn dotfolders_are_hidden() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "note.md");
        touch(tmp.path(), ".textree/order.json");
        touch(tmp.path(), ".git/config");
        let tree = build_tree(tmp.path()).unwrap();
        assert_eq!(tree.len(), 1, ".textree/.git are hidden");
        assert_eq!(tree[0].name, "note");
    }

    #[test]
    fn obsidian_vault_coexists_artifacts_hidden_but_preserved() {
        // Opening a vault shared with Obsidian must not surface or disturb its config (`.obsidian/`)
        // nor its JSON Canvas files (`.canvas`). The tree shows only the notes; the artifacts stay
        // untouched on disk so the two apps can take turns on the same vault losslessly.
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "note.md");
        touch(tmp.path(), ".obsidian/app.json");
        touch(tmp.path(), ".obsidian/workspace.json");
        touch(tmp.path(), "board.canvas");
        let tree = build_tree(tmp.path()).unwrap();
        assert_eq!(tree.len(), 1, "only the markdown note is shown");
        assert_eq!(tree[0].name, "note");
        // build_tree is read-only: the Obsidian artifacts remain exactly where they were.
        assert!(tmp.path().join(".obsidian/app.json").is_file());
        assert!(tmp.path().join("board.canvas").is_file());
    }

    #[test]
    fn root_level_note_matching_vault_name_is_not_dropped() {
        // TempDir itself uses a dot-prefixed (`.tmpXXXX`) name, so to avoid colliding with the
        // dot-hidden rule we create a separate non-dot subfolder to use as the vault root.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("vault");
        fs::create_dir_all(&root).unwrap();
        let root_name = root.file_name().unwrap().to_string_lossy().to_string();
        touch(&root, &format!("{root_name}.md"));
        let tree = build_tree(&root).unwrap();
        assert_eq!(
            tree.len(),
            1,
            "a note sharing the root's name must not be dropped"
        );
        assert_eq!(tree[0].name, root_name);
        assert_eq!(tree[0].kind, NodeKind::Leaf);
    }

    #[test]
    fn md_file_and_sibling_folder_with_same_stem_coexist() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "topics/note-a.md");
        touch(tmp.path(), "topics.md"); // not topics/topics.md → a leaf at the root
        let tree = build_tree(tmp.path()).unwrap();
        assert_eq!(tree.len(), 2);
        let container = tree.iter().find(|n| n.kind == NodeKind::Container).unwrap();
        assert_eq!(container.name, "topics");
        assert!(container.body_path.is_none());
        let leaf = tree.iter().find(|n| n.kind == NodeKind::Leaf).unwrap();
        assert_eq!(leaf.name, "topics");
    }
}

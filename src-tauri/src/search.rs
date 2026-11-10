//! Local body full-text search (P1b).
//!
//! Indexes `.md` body/title with a tantivy index. For Korean substring
//! matching it uses an n-gram (2~3) tokenizer, and the document identifier is
//! the vault relative path.
//! The index lives in the app data directory (vault stays clean) and is
//! separated per vault by hash.
//! "FS is truth, the index is a derived cache" — on corruption, recover by
//! rebuilding (`>reindex`).

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer};
use tantivy::{doc, Index, IndexWriter, TantivyDocument, Term};

const TOKENIZER: &str = "ngram";
const WRITER_HEAP: usize = 50_000_000;

/// Search hit sent to the front (app IPC wire type).
#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Vault relative path (POSIX separator).
    pub path: String,
    pub title: String,
    /// Body excerpt around the match.
    pub snippet: String,
    /// Highlight [start, end) char indices within the snippet string.
    pub ranges: Vec<(usize, usize)>,
}

/// Schema field handles (avoid re-lookup on every query/index).
#[derive(Clone, Copy)]
pub struct Fields {
    pub path: Field,
    pub title: Field,
    pub body: Field,
}

/// path=identifier (non-tokenized, for exact deletion), title/body=ngram tokens.
pub fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let path = b.add_text_field("path", STRING | STORED);
    let text = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER)
                .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let title = b.add_text_field("title", text.clone());
    let body = b.add_text_field("body", text);
    let schema = b.build();
    (schema, Fields { path, title, body })
}

/// Registers the ngram tokenizer on the index (required right after open/create).
pub fn register_tokenizer(index: &Index) {
    let ngram = NgramTokenizer::new(2, 3, false).expect("valid ngram parameters");
    let analyzer = TextAnalyzer::builder(ngram).filter(LowerCaser).build();
    index.tokenizers().register(TOKENIZER, analyzer);
}

/// What a vault root is known by: its canonical path where it resolves, the path as given where it
/// does not (a folder that is gone). Two spellings of one folder — a trailing separator, other
/// letter case on Windows, a link — come out the same, so this is what roots are compared by.
pub fn vault_identity(vault_root: &Path) -> PathBuf {
    std::fs::canonicalize(vault_root).unwrap_or_else(|_| vault_root.to_path_buf())
}

/// Vault root (canonical) → index directory under app data.
pub fn index_dir(app_data: &Path, vault_root: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(vault_identity(vault_root).to_string_lossy().as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    app_data.join("index").join(&hex[..16])
}

/// Relative path against the vault root as a POSIX-separator string.
fn rel_posix(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Uses the `.md` file name (without extension) as the title.
fn title_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Upsert one document: delete the doc with the same path and add anew (replace = idempotent).
pub fn upsert(
    writer: &IndexWriter,
    f: &Fields,
    rel_path: &str,
    title: &str,
    body: &str,
) -> tantivy::Result<()> {
    writer.delete_term(Term::from_field_text(f.path, rel_path));
    writer.add_document(doc!(
        f.path => rel_path,
        f.title => title,
        f.body => body,
    ))?;
    Ok(())
}

/// Delete a document by path (commit is the caller's responsibility).
pub fn delete(writer: &IndexWriter, f: &Fields, rel_path: &str) {
    writer.delete_term(Term::from_field_text(f.path, rel_path));
}

/// Recursively index all `.md` in the vault (commit is the caller's). Skips dot directories.
pub fn build_all(writer: &mut IndexWriter, f: &Fields, root: &Path) -> tantivy::Result<()> {
    index_dir_recursive(writer, f, root, root);
    Ok(())
}

fn index_dir_recursive(writer: &IndexWriter, f: &Fields, root: &Path, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|r| r.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // exclude .textree/.git etc.
        }
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            index_dir_recursive(writer, f, root, &path);
        } else if name.to_ascii_lowercase().ends_with(".md") {
            if let (Some(rel), Ok(body)) = (rel_posix(root, &path), std::fs::read_to_string(&path))
            {
                let _ = upsert(writer, f, &rel, &title_of(&path), &body);
            }
        }
    }
}

/// Multi-field query over body·title. Attaches a body snippet·highlight ranges to each hit.
/// Empty/whitespace query yields empty results.
pub fn search(
    index: &Index,
    f: &Fields,
    query: &str,
    limit: usize,
) -> tantivy::Result<Vec<SearchHit>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let parser = QueryParser::for_index(index, vec![f.title, f.body]);
    // Keep special characters in user input from causing parse errors — empty results on failure.
    let Ok(q) = parser.parse_query(query) else {
        return Ok(Vec::new());
    };
    let top = searcher.search(&q, &TopDocs::with_limit(limit).order_by_score())?;
    let mut snippet_gen = SnippetGenerator::create(&searcher, &q, f.body)?;
    snippet_gen.set_max_num_chars(160);

    let mut hits = Vec::with_capacity(top.len());
    for (_score, addr) in top {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let path = first_text(&doc, f.path);
        let title = first_text(&doc, f.title);
        let snip = snippet_gen.snippet_from_doc(&doc);
        let fragment = snip.fragment().to_string();
        let ranges = byte_ranges_to_char(&fragment, snip.highlighted());
        hits.push(SearchHit {
            path,
            title,
            snippet: fragment,
            ranges,
        });
    }
    Ok(hits)
}

fn first_text(doc: &TantivyDocument, field: Field) -> String {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// tantivy highlights (byte ranges) → **sorted·non-overlapping** char index ranges of the snippet string.
/// The ngram tokenizer produces multiple overlapping ranges per match, so this guarantees the
/// "sorted·disjoint" contract that consumers (front `highlight()`·publishing renderer) rely on.
/// Without merging, overlapping spans get re-sliced and characters are emitted twice.
/// For JS string slice (UTF-16 BMP) alignment. Supplementary characters (emoji) are approximate (negligible).
fn byte_ranges_to_char(s: &str, ranges: &[std::ops::Range<usize>]) -> Vec<(usize, usize)> {
    // Count via char_indices without slicing, so it never panics on mid-codepoint offsets.
    let byte_to_char = |b: usize| s.char_indices().take_while(|(i, _)| *i < b).count();
    let mut spans: Vec<(usize, usize)> = ranges
        .iter()
        .map(|r| (byte_to_char(r.start), byte_to_char(r.end)))
        .collect();
    // ngram matches are overlapping·unsorted → sort, then merge adjacent/overlapping ranges to guarantee disjoint.
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Index state of the current vault. Holds a long-lived IndexWriter to serialize build/incremental indexing.
pub struct IndexState {
    pub index: Index,
    pub writer: IndexWriter,
    pub fields: Fields,
    /// The vault this index is for (its [`vault_identity`]). Documents are keyed by paths relative
    /// to a vault root, so a note of one vault written into another's index would show up there
    /// as a note that does not exist, or replace what a same-named note of that vault says.
    vault: PathBuf,
}

impl IndexState {
    /// Opens the index of the vault at `vault_root` in the directory or (if absent) creates it.
    /// Registers the tokenizer.
    pub fn open_or_create(dir: &Path, vault_root: &Path) -> tantivy::Result<Self> {
        std::fs::create_dir_all(dir).ok();
        let (schema, fields) = build_schema();
        let mmap = MmapDirectory::open(dir)?;
        let index = Index::open_or_create(mmap, schema)?;
        register_tokenizer(&index);
        let writer = index.writer(WRITER_HEAP)?;
        Ok(Self {
            index,
            writer,
            fields,
            vault: vault_identity(vault_root),
        })
    }

    /// Whether this is the index of the vault at `vault_root`.
    ///
    /// A note can finish saving after the person has opened another folder (the save belongs to
    /// the folder it was typed in), and a watch or a first build of the folder opened before can
    /// still be running for a moment after the next one's index is in place. What they bring is
    /// for an index that is no longer installed; it is left out, not written into this one.
    pub fn serves(&self, vault_root: &Path) -> bool {
        vault_root == self.vault || vault_identity(vault_root) == self.vault
    }

    pub fn is_empty(&self) -> tantivy::Result<bool> {
        Ok(self.index.reader()?.searcher().num_docs() == 0)
    }

    /// Full rebuild: delete all existing documents, then re-index the vault. Refused for any vault
    /// but the one this index is for — its files would replace everything this index holds.
    pub fn rebuild(&mut self, vault_root: &Path) -> tantivy::Result<()> {
        if !self.serves(vault_root) {
            return Err(tantivy::TantivyError::InvalidArgument(format!(
                "the index is not for {}",
                vault_root.display()
            )));
        }
        self.writer.delete_all_documents()?;
        build_all(&mut self.writer, &self.fields, vault_root)?;
        self.writer.commit()?;
        Ok(())
    }

    /// Stages a single-file upsert/delete to the writer (commit is the batch caller's responsibility).
    /// Skipped when the change is in another vault (see [`IndexState::serves`]).
    pub fn apply_change(&mut self, vault_root: &Path, abs_path: &Path, removed: bool) {
        if !self.serves(vault_root) {
            log::debug!(
                "index: change in {} is for another vault; skipped",
                abs_path.display()
            );
            return;
        }
        if let Some(rel) = rel_posix(vault_root, abs_path) {
            if removed {
                delete(&self.writer, &self.fields, &rel);
            // On read failure, keep the existing index (stale) — self-heals via watcher re-fire/reindex.
            } else if let Ok(body) = std::fs::read_to_string(abs_path) {
                let _ = upsert(&self.writer, &self.fields, &rel, &title_of(abs_path), &body);
            }
        }
    }

    /// Indexes a single note from in-memory body and commits (in-app edit path — the watcher
    /// suppresses self-writes, so the index is updated deterministically at the write site).
    /// Skipped when the note is in another vault (see [`IndexState::serves`]): the index is a
    /// derived cache, and that vault's own index, like for any change made while it is not open,
    /// takes the note in when it is rebuilt.
    pub fn index_note(
        &mut self,
        vault_root: &Path,
        abs_path: &Path,
        body: &str,
    ) -> tantivy::Result<()> {
        if !self.serves(vault_root) {
            log::debug!(
                "index: {} is a note of another vault; skipped",
                abs_path.display()
            );
            return Ok(());
        }
        if let Some(rel) = rel_posix(vault_root, abs_path) {
            upsert(&self.writer, &self.fields, &rel, &title_of(abs_path), body)?;
            self.commit()?;
        }
        Ok(())
    }

    pub fn commit(&mut self) -> tantivy::Result<()> {
        self.writer.commit()?;
        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize) -> tantivy::Result<Vec<SearchHit>> {
        search(&self.index, &self.fields, query, limit)
    }
}

/// Current vault index handle managed as Tauri State. Shared (Arc) by the watcher thread and commands.
#[derive(Default)]
pub struct IndexHandle(pub Mutex<Option<IndexState>>);

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn num_docs(index: &Index) -> u64 {
        index.reader().unwrap().searcher().num_docs()
    }

    fn open_mem() -> (Index, Fields) {
        let (schema, f) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizer(&index);
        (index, f)
    }

    #[test]
    fn upsert_then_delete_roundtrips() {
        let (index, f) = open_mem();
        let mut w: IndexWriter = index.writer(WRITER_HEAP).unwrap();
        upsert(
            &w,
            &f,
            "journal/2026.md",
            "2026",
            "attached full-text search today",
        )
        .unwrap();
        w.commit().unwrap();
        assert_eq!(num_docs(&index), 1);

        // Same path upsert = replace (not a duplicate).
        upsert(&w, &f, "journal/2026.md", "2026", "revised body").unwrap();
        w.commit().unwrap();
        assert_eq!(num_docs(&index), 1, "same path is replaced, stays at 1 doc");

        delete(&w, &f, "journal/2026.md");
        w.commit().unwrap();
        assert_eq!(num_docs(&index), 0);
    }

    #[test]
    fn build_all_indexes_md_bodies_with_relative_posix_path() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("journal")).unwrap();
        std::fs::write(root.join("note.md"), "root body").unwrap();
        std::fs::write(root.join("journal/2026.md"), "journal body").unwrap();
        std::fs::write(root.join("drawing.png"), b"binary").unwrap(); // exclude non-md
        std::fs::create_dir_all(root.join(".textree")).unwrap();
        std::fs::write(root.join(".textree/cache.md"), "cache body").unwrap(); // dot directory → excluded from index

        let (index, f) = open_mem();
        let mut w = index.writer(WRITER_HEAP).unwrap();
        build_all(&mut w, &f, root).unwrap();
        w.commit().unwrap(); // build_all delegates commit to the caller — commit to make visible.
        assert_eq!(
            num_docs(&index),
            2,
            ".md under a dot directory is excluded, still 2 docs"
        );

        // Whether the relative path is stored with POSIX separators — checked indirectly via delete.
        delete(&w, &f, "journal/2026.md");
        w.commit().unwrap();
        assert_eq!(num_docs(&index), 1);
    }

    #[test]
    fn query_matches_a_substring_of_text_without_word_breaks() {
        let (index, f) = open_mem();
        let mut w = index.writer(WRITER_HEAP).unwrap();
        upsert(&w, &f, "a.md", "Note A", "全文検索の設計を今日おこなった").unwrap();
        upsert(&w, &f, "b.md", "Note B", "猫の写真を貼り付けた").unwrap();
        w.commit().unwrap();

        let hits = search(&index, &f, "検索", 10).unwrap();
        assert_eq!(hits.len(), 1, "the query matches only a.md");
        assert_eq!(hits[0].path, "a.md");
        assert!(
            hits[0].snippet.contains("検索"),
            "snippet contains the matched term"
        );
        let ranges = &hits[0].ranges;
        assert!(!ranges.is_empty(), "highlight ranges exist");
        // Consumers (front highlight) rely on sorted·non-overlapping ranges — if the ngram
        // overlapping output is not merged, characters get emitted twice (regression guard).
        let snip_len = hits[0].snippet.chars().count();
        for w in ranges.windows(2) {
            assert!(
                w[0].1 <= w[1].0,
                "ranges must be sorted·non-overlapping: {ranges:?}"
            );
        }
        for &(start, end) in ranges {
            assert!(
                start < end && end <= snip_len,
                "range is within snippet bounds: {ranges:?}"
            );
        }
    }

    #[test]
    fn empty_query_returns_no_hits() {
        let (index, f) = open_mem();
        let mut w = index.writer(WRITER_HEAP).unwrap();
        upsert(&w, &f, "a.md", "A", "body").unwrap();
        w.commit().unwrap();
        assert!(search(&index, &f, "   ", 10).unwrap().is_empty());
    }

    #[test]
    fn open_or_create_then_rebuild_finds_doc_end_to_end() {
        let vault = TempDir::new().unwrap();
        std::fs::write(vault.path().join("memo.md"), "全文検索の統合テスト").unwrap();
        let idx = TempDir::new().unwrap(); // index directory (simulates app data)

        let mut state = IndexState::open_or_create(idx.path(), vault.path()).unwrap();
        // A new index is empty → full build.
        state.rebuild(vault.path()).unwrap();

        let hits = state.search("検索", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "memo.md");
    }

    #[test]
    fn is_empty_reports_doc_presence() {
        let vault = TempDir::new().unwrap();
        let idx = TempDir::new().unwrap();
        let state = IndexState::open_or_create(idx.path(), vault.path()).unwrap();
        assert!(state.is_empty().unwrap(), "a new index is empty");
    }

    #[test]
    fn index_note_indexes_in_memory_body() {
        // In-app edit path regression guard: the in-memory body is indexed without re-reading from disk.
        let vault = TempDir::new().unwrap();
        let idx = TempDir::new().unwrap();
        let abs = vault.path().join("journal/today.md");

        let mut state = IndexState::open_or_create(idx.path(), vault.path()).unwrap();
        state
            .index_note(vault.path(), &abs, "本文のトークン")
            .unwrap();

        let hits = state.search("トークン", 10).unwrap();
        assert_eq!(hits.len(), 1, "in-memory body is found by search");
        assert_eq!(
            hits[0].path, "journal/today.md",
            "relative path (POSIX) identifier matches"
        );
    }

    /// The index of vault A, holding A's `same.md`, with vault B beside it holding its own
    /// `same.md` and `only-b.md`.
    fn two_vaults() -> (TempDir, TempDir, TempDir, IndexState) {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        std::fs::write(a.path().join("same.md"), "alpha body").unwrap();
        std::fs::write(b.path().join("same.md"), "bravo body").unwrap();
        std::fs::write(b.path().join("only-b.md"), "bravo only").unwrap();
        let idx = TempDir::new().unwrap();
        let mut state = IndexState::open_or_create(idx.path(), a.path()).unwrap();
        state.rebuild(a.path()).unwrap();
        (a, b, idx, state)
    }

    #[test]
    fn index_note_from_another_vault_leaves_the_index_alone() {
        let (_a, b, _idx, mut state) = two_vaults();

        state
            .index_note(b.path(), &b.path().join("only-b.md"), "bravo only")
            .unwrap();
        state
            .index_note(b.path(), &b.path().join("same.md"), "bravo body")
            .unwrap();

        assert_eq!(
            num_docs(&state.index),
            1,
            "no note of another vault shows up here"
        );
        assert!(
            state.search("bravo", 10).unwrap().is_empty(),
            "nor replaces a same-named note"
        );
        assert_eq!(state.search("alpha", 10).unwrap().len(), 1);
    }

    #[test]
    fn changes_from_another_vault_leave_the_index_alone() {
        let (_a, b, _idx, mut state) = two_vaults();

        state.apply_change(b.path(), &b.path().join("only-b.md"), false);
        state.apply_change(b.path(), &b.path().join("same.md"), true);
        state.commit().unwrap();

        assert_eq!(num_docs(&state.index), 1);
        assert_eq!(
            state.search("alpha", 10).unwrap().len(),
            1,
            "a same-named note is not removed"
        );
    }

    #[test]
    fn rebuild_from_another_vault_is_refused() {
        let (_a, b, _idx, mut state) = two_vaults();

        assert!(
            state.rebuild(b.path()).is_err(),
            "another vault's files do not fill this index"
        );

        assert_eq!(num_docs(&state.index), 1, "what the index held is kept");
        assert_eq!(state.search("alpha", 10).unwrap().len(), 1);
    }

    #[test]
    fn same_vault_spelled_differently_is_still_served() {
        let (a, _b, _idx, mut state) = two_vaults();
        // The same folder through a `..` detour: not the same string, the same folder.
        let detour = a.path().join("sub").join("..");
        std::fs::create_dir(a.path().join("sub")).unwrap();
        std::fs::write(a.path().join("more.md"), "alpha again").unwrap();

        assert!(state.serves(&detour));
        state.rebuild(&detour).unwrap();
        assert_eq!(num_docs(&state.index), 2);
    }

    #[test]
    fn schema_has_three_fields() {
        let (schema, f) = build_schema();
        assert_eq!(schema.get_field("path").unwrap(), f.path);
        assert_eq!(schema.get_field("title").unwrap(), f.title);
        assert_eq!(schema.get_field("body").unwrap(), f.body);
    }

    #[test]
    fn index_dir_is_stable_and_distinct_per_root() {
        let app = Path::new("/app");
        let a1 = index_dir(app, Path::new("/no/such/vault-a"));
        let a2 = index_dir(app, Path::new("/no/such/vault-a"));
        let b = index_dir(app, Path::new("/no/such/vault-b"));
        assert_eq!(a1, a2, "same root yields the same directory");
        assert_ne!(a1, b, "different roots yield different directories");
        assert!(a1.starts_with(app.join("index")));
    }
}

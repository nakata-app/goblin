//! Tantivy-based full-text index over blob metadata + content.
//!
//! Lives at `<workspace>/.aegis/blobs_index/`. Each indexed document
//! mirrors a blob in [`crate::blob_store::BlobStore`]:
//!
//! | Field        | Type           | Notes                               |
//! |--------------|----------------|-------------------------------------|
//! | `id`         | STRING+STORED  | Full BLAKE3 hex (joins to BlobStore)|
//! | `tool`       | STRING+STORED  | Originating tool name               |
//! | `source`     | TEXT+STORED    | URL or path, searchable + display   |
//! | `content`    | TEXT           | Body, searchable but NOT stored     |
//! | `created_at` | U64+INDEXED+FAST+STORED | Unix-epoch seconds         |
//!
//! Bodies are kept out of the index — they live in the blob store.
//! When a search hit comes back, the caller pulls the original from
//! `BlobStore::read(id)`. This keeps the index tight and avoids a
//! second copy of the same content.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, FAST, INDEXED, STORED, STRING, TEXT};
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, Term};
use thiserror::Error;

use crate::blob_store::{BlobId, BlobMeta};

const WRITER_HEAP_BYTES: usize = 50_000_000; // 50 MB — Tantivy minimum is ~15 MB.

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("tantivy error: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
    #[error("query parse error: {0}")]
    QueryParse(#[from] tantivy::query::QueryParserError),
    #[error("opendir error: {0}")]
    Open(#[from] tantivy::directory::error::OpenDirectoryError),
}

/// One result row from a search.
#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub id: BlobId,
    pub tool: String,
    pub source: Option<String>,
    pub created_at: u64,
    pub score: f32,
}

struct Schemas {
    id: Field,
    tool: Field,
    source: Field,
    content: Field,
    created_at: Field,
    schema: Schema,
}

impl Schemas {
    fn build() -> Self {
        let mut b = Schema::builder();
        let id = b.add_text_field("id", STRING | STORED);
        let tool = b.add_text_field("tool", STRING | STORED);
        let source = b.add_text_field("source", TEXT | STORED);
        let content = b.add_text_field("content", TEXT);
        let created_at = b.add_u64_field("created_at", INDEXED | STORED | FAST);
        let schema = b.build();
        Schemas {
            id,
            tool,
            source,
            content,
            created_at,
            schema,
        }
    }
}

pub struct BlobIndex {
    #[allow(dead_code)]
    path: PathBuf,
    index: Index,
    schemas: Schemas,
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
}

impl BlobIndex {
    /// Open or create the index at `<workspace>/.metis/blobs_index/`.
    pub fn open(workspace: &Path) -> Result<Self, IndexError> {
        let path = workspace.join(".metis").join("blobs_index");
        std::fs::create_dir_all(&path)?;

        let schemas = Schemas::build();

        // `Index::open_or_create` would be ideal but its semantics
        // around schema mismatches are unforgiving. Instead: try open,
        // fall back to create if the directory is empty.
        let index = match Index::open_in_dir(&path) {
            Ok(idx) => idx,
            Err(_) => Index::create_in_dir(&path, schemas.schema.clone())?,
        };

        let writer = index.writer(WRITER_HEAP_BYTES)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        Ok(Self {
            path,
            index,
            schemas,
            writer: Mutex::new(writer),
            reader,
        })
    }

    /// Index a blob. Caller is responsible for committing — call
    /// [`commit`] once after a batch, or use [`add_and_commit`] for
    /// one-shot indexing.
    pub fn add(&self, id: &BlobId, meta: &BlobMeta, content: &str) -> Result<(), IndexError> {
        let writer = self.writer.lock().expect("writer mutex poisoned");
        // Replace any prior doc with the same id, so re-indexing the
        // same blob doesn't create duplicates.
        writer.delete_term(Term::from_field_text(self.schemas.id, &id.0));

        let mut tdoc = doc!(
            self.schemas.id => id.0.clone(),
            self.schemas.tool => meta.tool.clone(),
            self.schemas.content => content.to_string(),
            self.schemas.created_at => meta.created_at,
        );
        if let Some(src) = &meta.source {
            tdoc.add_text(self.schemas.source, src);
        }
        writer.add_document(tdoc)?;
        Ok(())
    }

    pub fn commit(&self) -> Result<(), IndexError> {
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    pub fn add_and_commit(
        &self,
        id: &BlobId,
        meta: &BlobMeta,
        content: &str,
    ) -> Result<(), IndexError> {
        self.add(id, meta, content)?;
        self.commit()
    }

    /// Search across `source` + `content` with optional `tool:` filter.
    /// `query` may use Lucene-style operators via tantivy's parser.
    pub fn search(
        &self,
        query: &str,
        top_k: usize,
        tool_filter: Option<&str>,
    ) -> Result<Vec<SearchHit>, IndexError> {
        let searcher = self.reader.searcher();
        let parser =
            QueryParser::for_index(&self.index, vec![self.schemas.source, self.schemas.content]);
        let parsed = parser.parse_query(query)?;

        let final_query: Box<dyn Query> = match tool_filter {
            Some(tool) => {
                let tool_term = Term::from_field_text(self.schemas.tool, tool);
                let tool_q: Box<dyn Query> =
                    Box::new(TermQuery::new(tool_term, IndexRecordOption::Basic));
                Box::new(BooleanQuery::new(vec![
                    (Occur::Must, parsed),
                    (Occur::Must, tool_q),
                ]))
            }
            None => parsed,
        };

        let top = searcher.search(&final_query, &TopDocs::with_limit(top_k))?;

        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let retrieved: tantivy::TantivyDocument = searcher.doc(addr)?;
            hits.push(self.row_from_doc(retrieved, score));
        }
        Ok(hits)
    }

    fn row_from_doc(&self, doc: tantivy::TantivyDocument, score: f32) -> SearchHit {
        use tantivy::schema::Value;
        let id = doc
            .get_first(self.schemas.id)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let tool = doc
            .get_first(self.schemas.tool)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let source = doc
            .get_first(self.schemas.source)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let created_at = doc
            .get_first(self.schemas.created_at)
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        SearchHit {
            id: BlobId(id),
            tool,
            source,
            created_at,
            score,
        }
    }

    /// Drop a single document by blob id. Caller must commit afterward.
    pub fn delete(&self, id: &BlobId) -> Result<(), IndexError> {
        let writer = self.writer.lock().expect("writer mutex poisoned");
        writer.delete_term(Term::from_field_text(self.schemas.id, &id.0));
        Ok(())
    }

    pub fn delete_and_commit(&self, id: &BlobId) -> Result<(), IndexError> {
        self.delete(id)?;
        self.commit()
    }

    /// Total document count, used by `metis ctx stats`.
    pub fn doc_count(&self) -> Result<u64, IndexError> {
        let searcher = self.reader.searcher();
        Ok(searcher.num_docs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn idx() -> (TempDir, BlobIndex) {
        let tmp = TempDir::new().unwrap();
        let idx = BlobIndex::open(tmp.path()).unwrap();
        (tmp, idx)
    }

    fn blob(content: &str, tool: &str, source: Option<&str>) -> (BlobId, BlobMeta) {
        let id = BlobId::from_content(content.as_bytes());
        let mut meta = BlobMeta::new(tool);
        if let Some(s) = source {
            meta = meta.with_source(s);
        }
        meta.created_at = 1_700_000_000;
        (id, meta)
    }

    #[test]
    fn add_then_search_finds_doc() {
        let (_tmp, idx) = idx();
        let (id, meta) = blob("hello world tantivy", "bash", None);
        idx.add_and_commit(&id, &meta, "hello world tantivy")
            .unwrap();
        let hits = idx.search("tantivy", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
        assert_eq!(hits[0].tool, "bash");
    }

    #[test]
    fn search_ranks_by_relevance() {
        let (_tmp, idx) = idx();
        let (id1, m1) = blob("rust async tokio", "bash", None);
        let (id2, m2) = blob("rust rust rust async async tokio tokio", "bash", None);
        idx.add(&id1, &m1, "rust async tokio").unwrap();
        idx.add(&id2, &m2, "rust rust rust async async tokio tokio")
            .unwrap();
        idx.commit().unwrap();
        let hits = idx.search("rust", 10, None).unwrap();
        assert_eq!(hits.len(), 2);
        // Doc with more occurrences should outrank.
        assert_eq!(hits[0].id, id2);
    }

    #[test]
    fn tool_filter_restricts_results() {
        let (_tmp, idx) = idx();
        // Distinct content => distinct BlobIds, so the dedup-by-id
        // path doesn't drop one of them.
        let (id1, m1) = blob("matching content here from bash", "bash", None);
        let (id2, m2) = blob("matching content here from reader", "read_file", None);
        idx.add(&id1, &m1, "matching content here from bash")
            .unwrap();
        idx.add(&id2, &m2, "matching content here from reader")
            .unwrap();
        idx.commit().unwrap();
        let hits = idx.search("matching", 10, Some("bash")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tool, "bash");
    }

    #[test]
    fn source_field_is_searchable() {
        let (_tmp, idx) = idx();
        let (id, meta) = blob(
            "body about cats",
            "web_fetch",
            Some("https://example.com/dogs"),
        );
        idx.add_and_commit(&id, &meta, "body about cats").unwrap();
        let hits = idx.search("dogs", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source.as_deref(), Some("https://example.com/dogs"));
    }

    #[test]
    fn empty_search_returns_no_hits() {
        let (_tmp, idx) = idx();
        let hits = idx.search("nothing here", 10, None).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn delete_removes_doc() {
        let (_tmp, idx) = idx();
        let (id, meta) = blob("temporary", "bash", None);
        idx.add_and_commit(&id, &meta, "temporary").unwrap();
        assert_eq!(idx.doc_count().unwrap(), 1);
        idx.delete_and_commit(&id).unwrap();
        assert_eq!(idx.doc_count().unwrap(), 0);
    }

    #[test]
    fn re_indexing_same_id_does_not_duplicate() {
        let (_tmp, idx) = idx();
        let (id, meta) = blob("first", "bash", None);
        idx.add_and_commit(&id, &meta, "first").unwrap();
        idx.add_and_commit(&id, &meta, "first updated body")
            .unwrap();
        assert_eq!(idx.doc_count().unwrap(), 1);
        let hits = idx.search("updated", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn reopen_persists_documents() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_path_buf();
        {
            let idx = BlobIndex::open(&path).unwrap();
            let (id, meta) = blob("persistent body", "bash", None);
            idx.add_and_commit(&id, &meta, "persistent body").unwrap();
        }
        let idx2 = BlobIndex::open(&path).unwrap();
        let hits = idx2.search("persistent", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
    }
}

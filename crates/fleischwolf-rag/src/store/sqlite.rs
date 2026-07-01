//! SQLite vector store (feature `sqlite`, on by default).
//!
//! Uses the bundled SQLite that ships with `sqlx`. Embeddings are stored as a
//! little-endian `f32` BLOB; `vector_search` loads candidates and ranks them by
//! cosine in Rust — plenty for the eval-scale corpora this crate targets.

use super::{top_k_by_cosine, VectorStore};
use crate::math;
use crate::model::{Chunk, Document, Scored};
use crate::{RagError, Result};
use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use std::str::FromStr;

/// SQLite-backed store.
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Connect to (creating if missing) the SQLite database at `url`
    /// (e.g. `sqlite://data/rag.db` or `sqlite::memory:`).
    pub async fn connect(url: &str) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(url)
            .map_err(|e| RagError::config(format!("invalid RAG_DATABASE_URL '{url}': {e}")))?
            .create_if_missing(true);
        // SQLite won't create parent directories for the DB file; do it ourselves.
        let filename = opts.get_filename();
        if let Some(parent) = filename.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        Ok(SqliteStore { pool })
    }
}

fn row_to_chunk(row: &sqlx::sqlite::SqliteRow, with_embedding: bool) -> Result<(Chunk, Vec<f32>)> {
    let metadata: String = row.try_get("metadata")?;
    let emb_bytes: Vec<u8> = row.try_get("embedding")?;
    let embedding = math::from_bytes(&emb_bytes);
    let chunk = Chunk {
        id: row.try_get("id")?,
        doc_id: row.try_get("doc_id")?,
        ordinal: row.try_get("ordinal")?,
        text: row.try_get("text")?,
        token_count: row.try_get("token_count")?,
        metadata: serde_json::from_str(&metadata).unwrap_or(serde_json::Value::Null),
        embedding: if with_embedding { Some(embedding.clone()) } else { None },
    };
    Ok((chunk, embedding))
}

#[async_trait]
impl VectorStore for SqliteStore {
    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS documents (
                id TEXT PRIMARY KEY,
                source_uri TEXT NOT NULL,
                title TEXT NOT NULL,
                hash TEXT NOT NULL,
                metadata TEXT NOT NULL DEFAULT 'null',
                created_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_documents_hash ON documents(hash)")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS chunks (
                id TEXT PRIMARY KEY,
                doc_id TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
                ordinal INTEGER NOT NULL,
                text TEXT NOT NULL,
                token_count INTEGER NOT NULL,
                metadata TEXT NOT NULL DEFAULT 'null',
                embedding BLOB NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_chunks_doc ON chunks(doc_id)")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn upsert_document(&self, doc: &Document) -> Result<()> {
        sqlx::query(
            "INSERT INTO documents (id, source_uri, title, hash, metadata, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                source_uri = excluded.source_uri,
                title = excluded.title,
                hash = excluded.hash,
                metadata = excluded.metadata",
        )
        .bind(&doc.id)
        .bind(&doc.source_uri)
        .bind(&doc.title)
        .bind(&doc.hash)
        .bind(serde_json::to_string(&doc.metadata)?)
        .bind(&doc.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_document_by_hash(&self, hash: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT id FROM documents WHERE hash = ?1 LIMIT 1")
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("id")))
    }

    async fn insert_chunks(&self, chunks: &[Chunk]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for c in chunks {
            let emb = c
                .embedding
                .as_ref()
                .ok_or_else(|| RagError::Store(format!("chunk {} has no embedding", c.id)))?;
            sqlx::query(
                "INSERT INTO chunks (id, doc_id, ordinal, text, token_count, metadata, embedding)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .bind(&c.id)
            .bind(&c.doc_id)
            .bind(c.ordinal)
            .bind(&c.text)
            .bind(c.token_count)
            .bind(serde_json::to_string(&c.metadata)?)
            .bind(math::to_bytes(emb))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn vector_search(&self, query: &[f32], k: usize) -> Result<Vec<Scored>> {
        let rows = sqlx::query(
            "SELECT id, doc_id, ordinal, text, token_count, metadata, embedding FROM chunks",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut candidates = Vec::with_capacity(rows.len());
        for row in &rows {
            let (chunk, emb) = row_to_chunk(row, false)?;
            candidates.push((chunk, emb));
        }
        Ok(top_k_by_cosine(query, candidates, k))
    }

    async fn all_chunks(&self) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT id, doc_id, ordinal, text, token_count, metadata, embedding FROM chunks",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| row_to_chunk(r, false).map(|(c, _)| c)).collect()
    }

    async fn count_chunks(&self) -> Result<usize> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM chunks").fetch_one(&self.pool).await?;
        Ok(row.get::<i64, _>("n") as usize)
    }

    async fn count_documents(&self) -> Result<usize> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM documents").fetch_one(&self.pool).await?;
        Ok(row.get::<i64, _>("n") as usize)
    }

    async fn clear(&self) -> Result<()> {
        sqlx::query("DELETE FROM chunks").execute(&self.pool).await?;
        sqlx::query("DELETE FROM documents").execute(&self.pool).await?;
        Ok(())
    }
}

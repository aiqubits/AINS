//! 用户记忆系统（AINS_PLAN 第四章）：KV / Vector / Document 三层，
//! 存储后端按平台选择（Native = redb，Web = IndexedDB）。

pub mod document;
pub mod engine;
pub mod extract;
pub mod in_memory;
pub mod kv;
pub mod kv_crypto;
pub mod manage;
pub mod memdir;
pub mod parser;
pub mod service;
pub mod stores;
pub mod ttl;
pub mod vector;
pub mod vector_manager;

#[cfg(not(target_arch = "wasm32"))]
pub mod kv_native;
#[cfg(not(target_arch = "wasm32"))]
pub mod vector_native;

#[cfg(target_arch = "wasm32")]
pub mod kv_web;
#[cfg(target_arch = "wasm32")]
pub mod vector_web;

pub use document::{
    DocumentChunk, DocumentMeta, DocumentStore, EMBED_BATCH_MAX, LocalDocumentStore, SearchResult,
};
pub use engine::MemoryEngine;
pub use extract::{
    EXTRACTION_SYSTEM_PROMPT, ExtractionOutcome, MAX_RECENT_LINES, MAX_SESSION_MEMORY_CHARS,
    MemoryExtractor, SESSION_MEMORY_KEY, SessionCheckpoint, build_session_memory,
    format_transcript, load_session_checkpoint, parse_memory_records, save_session_checkpoint,
};
pub use in_memory::InMemoryKvStore;
pub use kv::{
    KvStore, TABLE_DOCUMENTS, TABLE_EMBEDDINGS, TABLE_HNSW_CACHE, TABLE_KV, TABLE_MEMORIES, now_ms,
};
pub use kv_crypto::{EncryptedKvStore, EncryptionKey};
pub use memdir::{
    ENTRY_PREFIX, INDEX_KEY, MAX_ENTRYPOINT_BYTES, MAX_ENTRYPOINT_LINES, MAX_MANIFEST_FILES,
    MEMORY_POLICY_LINES, MemdirEntry, MemdirStore, MemoryScope, MemoryType, NewMemoryEntry,
    SCHEMA_VERSION, format_iso_utc, generate_memory_id, parse_iso_utc, slugify,
};
pub use parser::{DocumentKind, MAX_CHUNK_CHARS, chunk_document, extract_pdf_text};
pub use service::{
    DurableMemoryManifestItem, DurableMemoryMetadata, EmbeddingContract, ExtractionReason,
    ExtractionState, ExtractionToken, MemoryContext, MemoryHit, MemoryService, MemoryServiceConfig,
    SessionMemoryClearOutcome, build_durable_library_manifest_items, build_durable_manifest,
    build_durable_manifest_items, extract_digest, is_visible, owner_key_for_id,
};
pub use stores::{MemoryBackend, MemoryStores, open_memory_stores, prepare_encryption};
pub use ttl::{DEFAULT_SWEEP_INTERVAL, SweeperHandle, spawn_ttl_sweeper};
pub use vector::{
    MemoryEntry, MemoryNamespace, Metric, VECTOR_MAX_ENTRIES, VECTOR_MAX_ENTRIES_WEB, VectorIndex,
    VectorIndexConfig, VectorIndexManager, cosine_similarity, similarity_score, vector_from_value,
    vector_max_entries, vector_to_value,
};
pub use vector_manager::DefaultVectorIndexManager;

#[cfg(not(target_arch = "wasm32"))]
pub use kv_native::{RedbBackend, RedbKvStore};
#[cfg(not(target_arch = "wasm32"))]
pub use vector_native::{HNSW_CACHE_VERSION, HnswCacheMeta, HnswVectorIndex};

#[cfg(target_arch = "wasm32")]
pub use kv_web::{IndexedDbBackend, IndexedDbKvStore};
#[cfg(target_arch = "wasm32")]
pub use vector_web::LinearVectorIndex;

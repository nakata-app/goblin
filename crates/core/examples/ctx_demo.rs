//! End-to-end smoke test for the ctx blob store + index.
//!
//! Reads a real file from disk, stores it through `BlobStore`, indexes
//! it through `BlobIndex`, and runs a few searches. Reports compression
//! ratio, store/search latency, and the stashed reference summary the
//! agent loop would have seen instead of the raw content.
//!
//! Usage:
//!   cargo run -p aegis-core --example ctx_demo --
//!     <path-to-file> [<query>]

use std::path::PathBuf;
use std::time::Instant;

use aegis_core::{BlobMeta, BlobStore};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Args: <path-to-file> [<query>] [--workspace <dir>]
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut target: Option<String> = None;
    let mut query = "shot".to_string();
    let mut ws_arg: Option<PathBuf> = None;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--workspace" => {
                i += 1;
                ws_arg = Some(PathBuf::from(&argv[i]));
            }
            other if target.is_none() => target = Some(other.to_string()),
            other => query = other.to_string(),
        }
        i += 1;
    }
    let target = target.ok_or("usage: ctx_demo <path-to-file> [<query>] [--workspace <dir>]")?;

    let target = PathBuf::from(target);
    let raw = std::fs::read(&target)?;
    println!(
        "→ loaded {}: {} bytes ({} lines)",
        target.display(),
        raw.len(),
        std::str::from_utf8(&raw)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    );

    // Persist if --workspace given; otherwise use a tempdir.
    let _tmp_holder;
    let workspace_path: PathBuf = match ws_arg {
        Some(p) => {
            std::fs::create_dir_all(&p)?;
            p
        }
        None => {
            let t = tempfile::tempdir()?;
            let p = t.path().to_path_buf();
            _tmp_holder = t; // keep alive until end of main
            p
        }
    };
    println!("→ workspace: {}", workspace_path.display());

    // Store
    let t = Instant::now();
    let store = BlobStore::open(&workspace_path)?;
    let mut meta = BlobMeta::new("read_file");
    meta = meta.with_source(target.display().to_string());
    let id = store.store(&raw, meta)?;
    let store_ms = t.elapsed().as_millis();

    let (_, m) = store.read(&id)?;
    println!(
        "→ stored {} ({:.1}% on disk{}) in {} ms",
        id.reference(),
        100.0 * m.stored_size as f64 / m.original_size.max(1) as f64,
        if m.compressed { ", zstd" } else { "" },
        store_ms,
    );
    println!(
        "  saved: {} bytes",
        m.original_size.saturating_sub(m.stored_size)
    );

    // Index
    let t = Instant::now();
    let index = aegis_core::BlobIndex::open(&workspace_path)?;
    let body = String::from_utf8_lossy(&raw);
    index.add_and_commit(&id, &m, &body)?;
    let idx_ms = t.elapsed().as_millis();
    println!("→ indexed in {} ms ({} doc(s))", idx_ms, index.doc_count()?);

    // Search
    let t = Instant::now();
    let hits = index.search(&query, 5, None)?;
    let q_ms = t.elapsed().as_millis();
    println!(
        "→ search {:?} in {} ms — {} hit(s)",
        query,
        q_ms,
        hits.len()
    );
    for h in &hits {
        println!(
            "    {}  tool={}  score={:.2}  src={}",
            &h.id.0[..16],
            h.tool,
            h.score,
            h.source.as_deref().unwrap_or("(none)")
        );
    }

    // Stats
    let stats = store.stats()?;
    println!(
        "→ stats: {} blob(s), original={} on-disk={}",
        stats.blob_count, stats.total_original_bytes, stats.total_stored_bytes
    );

    // What the agent would have seen if this content came from a tool.
    println!("\n--- what the agent loop would have seen ---");
    println!(
        "[stashed: {} — {} bytes, {} lines]\n--- preview ---",
        id.reference(),
        m.original_size,
        body.lines().count()
    );
    let preview_end = body
        .char_indices()
        .nth(400)
        .map(|(i, _)| i)
        .unwrap_or(body.len());
    println!(
        "{}…\n[full body: cargo run ... -- {} ctx show {}]",
        &body[..preview_end],
        target.display(),
        &id.0[..16],
    );

    Ok(())
}

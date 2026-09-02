//! Content-addressed blob storage above `blob_threshold`, ARCHITECTURE §8.2.
//!
//! Below the threshold, a payload stays inline as `TEXT` in `run.db` — this
//! module is never involved for it ([`crate::store::CachedResponse::inline`],
//! S2). Above it, the payload moves to `blobs/b3/<hash>` beside `run.db`
//! (`b3` = blake3, §8.2's own layout), and the row keeps only the hash and size
//! ([`crate::store::CachedResponse::response_hash`]/`size_bytes`). The one
//! ordering rule, not negotiable:
//!
//! ```text
//! write blob → fsync → THEN commit the row
//! ```
//!
//! Never the reverse. A blob with no row is garbage collectable; a row with no
//! blob is corruption. `blob_threshold` itself has no fixed home in either spec
//! file beyond "the blob store" (PLAN_DEVIATIONS.md D5) — [`DEFAULT_BLOB_THRESHOLD_BYTES`]
//! is defined here, in the crate D5 assigns it to.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// §8.2: "Above `blob_threshold` (default 128 KB)".
pub const DEFAULT_BLOB_THRESHOLD_BYTES: usize = 128 * 1024;

/// A committed blob's identity: content hash (blake3, hex) and byte size — what a
/// row keeps once its payload has moved out-of-line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// §8.2's threshold decision: does this payload move out-of-line? Takes the
/// threshold as a parameter rather than reading a `Config` type — this crate's
/// only dependency is the trait seam `arbiter-kernel` defines (D1), and a raw
/// byte comparison is all this decision needs.
pub fn should_use_blob(payload_len: usize, blob_threshold: usize) -> bool {
    payload_len > blob_threshold
}

/// `<run_dir>/blobs/b3/<hash>` — the fixed on-disk layout for one run's blobs
/// (§8.2, §8.5's `runs/<id>/blobs/*`).
pub fn blob_path(run_dir: &Path, hash: &str) -> PathBuf {
    run_dir.join("blobs").join("b3").join(hash)
}

/// Writes `data` content-addressed under `run_dir`, **fsyncing before
/// returning**. The caller must not commit the row this blob backs until this
/// call has returned `Ok` — that ordering, not this function, is what §8.2's
/// rule protects; this function only guarantees its own half of it.
///
/// Content-addressed, so a hash that already exists on disk is left untouched
/// rather than rewritten: identical content hashes identically, and the
/// existing file already satisfies "written and fsynced."
pub fn write_blob(run_dir: &Path, data: &[u8]) -> Result<BlobRef, BlobError> {
    let hash = blake3::hash(data).to_hex().to_string();
    let dir = run_dir.join("blobs").join("b3");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(&hash);

    if !path.exists() {
        // Write under a process-unique temporary name, fsync its contents, then
        // rename into place and fsync the directory entry — so a crash mid-write
        // never leaves a half-written file visible under the final hash-named
        // path for a concurrent reader (or this same function, called again) to
        // find.
        let tmp = dir.join(format!(".{hash}.tmp.{}", std::process::id()));
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(data)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        std::fs::File::open(&dir)?.sync_all()?;
    }

    Ok(BlobRef {
        hash,
        size: data.len() as u64,
    })
}

/// Reads a previously written blob back, for a `run.db` row that references it.
pub fn read_blob(run_dir: &Path, hash: &str) -> Result<Vec<u8>, BlobError> {
    Ok(std::fs::read(blob_path(run_dir, hash))?)
}

/// One `doctor --gc` sweep's result: every blob deleted, and the bytes
/// reclaimed — "`arbiter doctor --gc` deletes blobs not named by any committed
/// `cache_entries` or `artifacts` row... and reports the bytes reclaimed" (§8.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    pub deleted_hashes: Vec<String>,
    pub bytes_reclaimed: u64,
}

/// Sweeps `run_dir`'s blobs, content-addressed so no refcounting is needed:
/// anything not in `referenced` is deletable. The caller supplies `referenced`
/// (every hash named by a committed `cache_entries` or `artifacts` row) rather
/// than this function querying `run.db` itself — those projection tables don't
/// exist in `run.db` yet (PLAN_DEVIATIONS.md D21; S4 adds them), so this keeps
/// the sweep correct and testable against the on-disk contract today,
/// independent of when S4 lands.
///
/// Never call this for a run whose lease may be live — [`gc_run`] enforces that
/// by construction; call this directly only once liveness has already been
/// checked (e.g. from a test).
pub fn gc_one_run(run_dir: &Path, referenced: &BTreeSet<String>) -> Result<GcReport, BlobError> {
    let dir = run_dir.join("blobs").join("b3");
    let mut report = GcReport::default();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // A `.<hash>.tmp.<pid>` in-flight write, from this or another process's
        // still-running write_blob -- never this sweep's to touch.
        if name.starts_with('.') {
            continue;
        }
        if referenced.contains(name) {
            continue;
        }
        let size = entry.metadata()?.len();
        std::fs::remove_file(entry.path())?;
        report.deleted_hashes.push(name.to_string());
        report.bytes_reclaimed += size;
    }
    Ok(report)
}

/// §8.2's liveness predicate, restated for a reader: "`doctor` is a reader and
/// cannot take a lease to find out, so it must apply the *same* liveness
/// predicate `reopen` uses (INTERFACES §1) rather than a simpler one." Reads the
/// single `run` row from an already-open `run.db` connection and asks the exact
/// question [`crate::lease::reopen`] asks before granting a steal.
pub fn is_run_lease_live(
    conn: &rusqlite::Connection,
    run_id: &str,
    current_boot_id: &str,
) -> Result<bool, BlobError> {
    use rusqlite::OptionalExtension;
    let row: Option<(u32, String)> = conn
        .query_row(
            "SELECT owner_pid, boot_id FROM run WHERE run_id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((recorded_pid, recorded_boot_id)) = row else {
        return Ok(false);
    };
    Ok(!crate::lease::owner_is_gone(
        &recorded_boot_id,
        recorded_pid,
        current_boot_id,
    ))
}

/// One run's GC, lease-checked: `None` if the run's lease is live (the run is
/// skipped entirely, per §8.2's "a blob fsynced before its row commits is
/// indistinguishable from an orphan and is not one"), `Some(report)` otherwise.
pub fn gc_run(
    conn: &rusqlite::Connection,
    run_id: &str,
    run_dir: &Path,
    referenced: &BTreeSet<String>,
    current_boot_id: &str,
) -> Result<Option<GcReport>, BlobError> {
    if is_run_lease_live(conn, run_id, current_boot_id)? {
        return Ok(None);
    }
    Ok(Some(gc_one_run(run_dir, referenced)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::{self, Owner};
    use crate::schema::open_run_db;
    use rusqlite::Connection;

    fn temp_run_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arbiter_blob_test_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_payload_under_threshold_stays_inline() {
        assert!(!should_use_blob(1024, DEFAULT_BLOB_THRESHOLD_BYTES));
        assert!(!should_use_blob(
            DEFAULT_BLOB_THRESHOLD_BYTES,
            DEFAULT_BLOB_THRESHOLD_BYTES
        ));
        assert!(should_use_blob(
            DEFAULT_BLOB_THRESHOLD_BYTES + 1,
            DEFAULT_BLOB_THRESHOLD_BYTES
        ));
    }

    #[test]
    fn row_never_references_a_missing_blob() {
        // §8.2's whole ordering rule, restated as a test: by the time write_blob
        // returns Ok, the blob a row is about to reference already exists on
        // disk, fsynced -- a row is never committed pointing at nothing.
        let dir = temp_run_dir("missing_blob");
        let data = b"a provider response well over the threshold, for this test's purposes";
        let blob_ref = write_blob(&dir, data).unwrap();

        assert!(blob_path(&dir, &blob_ref.hash).exists());
        assert_eq!(blob_ref.size, data.len() as u64);
        assert_eq!(read_blob(&dir, &blob_ref.hash).unwrap(), data);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writing_the_same_content_twice_is_idempotent() {
        let dir = temp_run_dir("idempotent");
        let data = b"identical content hashes identically";
        let a = write_blob(&dir, data).unwrap();
        let b = write_blob(&dir, data).unwrap();
        assert_eq!(a, b);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gc_deletes_only_unreferenced_blobs() {
        let dir = temp_run_dir("gc_sweep");
        let kept = write_blob(&dir, b"kept, still referenced").unwrap();
        let orphan = write_blob(&dir, b"orphan, no row references this").unwrap();

        let referenced = BTreeSet::from([kept.hash.clone()]);
        let report = gc_one_run(&dir, &referenced).unwrap();

        assert_eq!(report.deleted_hashes, vec![orphan.hash.clone()]);
        assert_eq!(report.bytes_reclaimed, orphan.size);
        assert!(blob_path(&dir, &kept.hash).exists());
        assert!(!blob_path(&dir, &orphan.hash).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gc_skips_a_run_with_a_live_lease() {
        let dir = temp_run_dir("gc_live_lease");
        let orphan = write_blob(&dir, b"written and fsynced, row not committed yet").unwrap();

        let conn = Connection::open_in_memory().unwrap();
        open_run_db(&conn, "0.1.0", "2026-09-02T00:00:00Z").unwrap();
        // Owned by *this* process, on *this* boot -- unambiguously live, exactly
        // lease.rs's own `a_live_owner_cannot_be_stolen` fixture.
        let live = Owner {
            pid: std::process::id(),
            boot_id: lease::boot_id(),
            hostname: "test-host".to_string(),
        };
        lease::create(&conn, "run_1", &live).unwrap();

        let referenced = BTreeSet::new();
        let result = gc_run(&conn, "run_1", &dir, &referenced, &lease::boot_id()).unwrap();

        assert!(
            result.is_none(),
            "a live-leased run must be skipped entirely, not swept"
        );
        assert!(
            blob_path(&dir, &orphan.hash).exists(),
            "a blob fsynced before its row commits is indistinguishable from an \
             orphan and is not one -- gc must not delete it while the lease is live"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gc_sweeps_a_run_whose_lease_is_gone() {
        let dir = temp_run_dir("gc_dead_lease");
        let orphan = write_blob(&dir, b"orphaned for real this time").unwrap();

        let conn = Connection::open_in_memory().unwrap();
        open_run_db(&conn, "0.1.0", "2026-09-02T00:00:00Z").unwrap();
        // A previous boot's pid -- unambiguously gone, matching lease.rs's own
        // `stale_boot_id_is_stealable` fixture.
        let dead = Owner {
            pid: 999_999,
            boot_id: "a-previous-boot".to_string(),
            hostname: "test-host".to_string(),
        };
        lease::create(&conn, "run_1", &dead).unwrap();

        let referenced = BTreeSet::new();
        let result = gc_run(&conn, "run_1", &dir, &referenced, &lease::boot_id()).unwrap();

        let report = result.expect("a run with a gone owner must be swept");
        assert_eq!(report.deleted_hashes, vec![orphan.hash]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

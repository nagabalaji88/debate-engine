//! F2 — `prompt_pack_mismatch` (G1), ARCHITECTURE §18's CI suite: "replay
//! under a different pack is refused without `--repack`."
//!
//! `PromptPack::verify_pack_hash` is the mechanism INTERFACES §23 names
//! ("exact replay refuses a differing `pack_hash`") -- what this fixture
//! proves. The `--repack` override flag itself is `arbiter replay`'s own
//! CLI argument (L3), which lives in `arbiter-cli` and so is outside this
//! crate's reach (the dependency rule, X2's own test).

use arbiter_kernel::prompt::{PromptError, PromptPack};
use std::path::Path;

fn temp_pack_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "arbiter_fixtures_pack_{label}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).unwrap();
}

fn write_manifest(dir: &Path, name: &str, version: &str) {
    write(
        dir,
        "manifest.toml",
        &format!("name = \"{name}\"\nversion = \"{version}\"\n"),
    );
}

/// A run's manifest records the pack hash it was actually run under
/// (`Manifest::pack_hash`, frozen at `init`). If replay loads a pack whose
/// content has since changed -- a different template body, here -- its own
/// freshly-computed hash no longer matches what the manifest recorded, and
/// `verify_pack_hash` must refuse rather than silently replay against
/// drifted prompts. The identical-content case must still replay cleanly.
#[test]
fn prompt_pack_mismatch() {
    let dir_original = temp_pack_dir("original");
    write_manifest(&dir_original, "default", "v1");
    write(
        &dir_original,
        "claims.extract.md",
        "---\nvariables = []\n---\noriginal wording\n",
    );
    let pack_at_run_time = PromptPack::load(&dir_original).unwrap();
    let recorded_pack_hash = pack_at_run_time.hash.as_str().to_string();

    let dir_edited = temp_pack_dir("edited");
    write_manifest(&dir_edited, "default", "v1");
    write(
        &dir_edited,
        "claims.extract.md",
        "---\nvariables = []\n---\nedited wording, same stage\n",
    );
    let pack_at_replay_time = PromptPack::load(&dir_edited).unwrap();

    let result = pack_at_replay_time.verify_pack_hash(&recorded_pack_hash);
    assert!(
        matches!(result, Err(PromptError::PackMismatch { .. })),
        "replaying under a pack whose content has drifted must be refused, never silently accepted"
    );

    assert!(
        pack_at_run_time
            .verify_pack_hash(&recorded_pack_hash)
            .is_ok(),
        "replaying under the exact same pack that produced the recorded hash must succeed"
    );

    let _ = std::fs::remove_dir_all(&dir_original);
    let _ = std::fs::remove_dir_all(&dir_edited);
}

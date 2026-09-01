//! Enforces ARCHITECTURE.md §4.1's dependency rule directly against `cargo
//! metadata`, so a `Cargo.toml` edit that violates it fails CI rather than
//! surfacing as a mystery months later.
//!
//! > core depends on nothing internal; kernel depends on core; everything else
//! > depends on kernel; nothing depends on cli.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

/// name -> set of arbiter-* crates it depends on (normal dependencies only —
/// dev-dependencies are test scaffolding, not the production dependency graph
/// the rule governs).
fn workspace_graph() -> BTreeMap<String, BTreeSet<String>> {
    let out = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .expect("cargo metadata must run — this test needs a working cargo on PATH");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let meta: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("cargo metadata must emit valid JSON");
    let packages = meta["packages"]
        .as_array()
        .expect("metadata.packages must be an array");

    let mut graph = BTreeMap::new();
    for pkg in packages {
        let name = pkg["name"].as_str().unwrap().to_string();
        if !name.starts_with("arbiter-") {
            continue; // a transitive third-party dependency, not a workspace member
        }
        let deps: BTreeSet<String> = pkg["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["kind"].is_null()) // null = normal; "dev"/"build" excluded
            .map(|d| d["name"].as_str().unwrap().to_string())
            .filter(|n| n.starts_with("arbiter-"))
            .collect();
        graph.insert(name, deps);
    }
    graph
}

#[test]
fn core_depends_on_nothing_internal() {
    let g = workspace_graph();
    let deps = g
        .get("arbiter-core")
        .expect("arbiter-core must exist in the workspace");
    assert!(
        deps.is_empty(),
        "arbiter-core must depend on no arbiter-* crate (ARCHITECTURE §4.1); found {deps:?}"
    );
}

#[test]
fn kernel_depends_on_core_only() {
    let g = workspace_graph();
    let deps = g
        .get("arbiter-kernel")
        .expect("arbiter-kernel must exist in the workspace");
    let expected: BTreeSet<String> = ["arbiter-core".to_string()].into_iter().collect();
    assert_eq!(
        deps, &expected,
        "arbiter-kernel must depend on arbiter-core and nothing else internal \
         (ARCHITECTURE §4.1: kernel owns the Store/Provider trait seams — see \
         PLAN_DEVIATIONS.md D1). Found {deps:?}"
    );
}

#[test]
fn everything_else_depends_on_kernel() {
    let g = workspace_graph();
    for (name, deps) in &g {
        if name == "arbiter-core" || name == "arbiter-kernel" {
            continue;
        }
        if name == "arbiter-workspace-checks" {
            continue; // this crate itself is not part of the production graph
        }
        assert!(
            deps.contains("arbiter-kernel"),
            "{name} must depend on arbiter-kernel (ARCHITECTURE §4.1: \
             \"everything else depends on kernel\"); its deps are {deps:?}"
        );
    }
}

#[test]
fn nothing_depends_on_cli() {
    let g = workspace_graph();
    for (name, deps) in &g {
        assert!(
            !deps.contains("arbiter-cli"),
            "{name} depends on arbiter-cli, which ARCHITECTURE §4.1 forbids: \
             \"nothing depends on cli\""
        );
    }
}

#[test]
fn no_dependency_cycle() {
    // A cheap corollary of the three rules above, but worth asserting directly:
    // if any crate can reach itself, the rules were satisfied locally while a
    // cycle still exists (e.g. two crates each individually depending on kernel
    // while also depending on each other).
    let g = workspace_graph();
    fn reaches(
        g: &BTreeMap<String, BTreeSet<String>>,
        start: &str,
        target: &str,
        seen: &mut BTreeSet<String>,
    ) -> bool {
        if !seen.insert(start.to_string()) {
            return false;
        }
        for d in g.get(start).into_iter().flatten() {
            if d == target || reaches(g, d, target, seen) {
                return true;
            }
        }
        false
    }
    for name in g.keys() {
        let mut seen = BTreeSet::new();
        for dep in g.get(name).into_iter().flatten() {
            assert!(
                !reaches(&g, dep, name, &mut seen),
                "cycle: {name} -> {dep} -> ... -> {name}"
            );
        }
    }
}

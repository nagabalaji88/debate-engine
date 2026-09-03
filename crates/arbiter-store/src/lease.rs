//! Run ownership: a compare-and-swap on `lease_epoch`, not a liveness check.
//! INTERFACES §1: "Liveness is not a lease... The liveness check therefore only
//! decides whether a steal is *permitted*; a monotonic `lease_epoch` decides who
//! *wins*."
//!
//! Linux, macOS, and Windows each get a real, native liveness + boot-id check
//! below — neither spec file discusses any platform beyond what this workspace's
//! CI runs (`ubuntu-latest`), so macOS/Windows are a deliberate addition past
//! spec, not a spec requirement (PLAN_DEVIATIONS.md D50). `#![forbid(unsafe_code)]`
//! rules out the traditional `kill(pid, 0)` libc call everywhere, so each
//! platform gets the safe, dependency-free equivalent already available to it —
//! see [`pid_is_alive`] and [`boot_id`] below for what each one actually does.
//!
//! This is a hard compile-time gate on every *other* platform, not a comment,
//! because the failure mode of skipping it is silent and actively unsafe rather
//! than merely broken: without a real liveness check, [`pid_is_alive`] would
//! always return `false` and [`owner_is_gone`] would then report *every* lease
//! as abandoned — including one held by a live process on the same machine and
//! boot. A second `reopen` would then successfully steal it via the epoch CAS,
//! and two processes would both believe they own the run: exactly the failure
//! INTERFACES §1's epoch design exists to prevent, reintroduced through an
//! OS-specific precondition quietly returning the wrong answer instead of
//! refusing to build.
//!
//! Every non-Linux liveness check below fails *closed* on any ambiguous result
//! (the OS command couldn't run, its output didn't parse, a permission error
//! instead of a clean "no such process") by reporting the pid as still alive —
//! a lease that should have been reclaimed but wasn't is a stuck run; a lease
//! stolen out from under its live owner is silent corruption. Only Linux's
//! `/proc/<pid>` check, verified against this workspace's own CI, is trusted to
//! resolve every case on its own.

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!(
    "arbiter_store::lease has a real liveness check for linux, macos, and windows \
     only. pid_is_alive() would always return false on any other platform, which \
     makes every lease look abandoned and stealable regardless of whether its \
     owner is still running — silently unsafe, not just unsupported. Porting this \
     module to another OS means implementing a real liveness check for that OS, \
     not removing this gate."
);

use rusqlite::{Connection, OptionalExtension};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub pid: u32,
    pub boot_id: String,
    pub hostname: String,
}

impl Owner {
    /// This process, as it would be recorded in the `run` table.
    pub fn current() -> Self {
        Self {
            pid: std::process::id(),
            boot_id: boot_id(),
            hostname: hostname(),
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// macOS's own per-boot-session UUID, regenerated on every boot — exactly
/// `/proc/sys/kernel/random/boot_id`'s job. `sysctl -n` is the safe,
/// dependency-free command-line form of the same value `sysctlbyname(3)`
/// returns; `#![forbid(unsafe_code)]` rules out calling that directly.
#[cfg(target_os = "macos")]
pub(crate) fn boot_id() -> String {
    std::process::Command::new("sysctl")
        .args(["-n", "kern.bootsessionuuid"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Windows has no single "boot id" file; `LastBootUpTime` is stable for one
/// boot and changes on every restart, which is all this needs — an opaque
/// string that is the *same* across one boot and *different* across any
/// other. `Get-CimInstance` (not the deprecated `wmic`, removed by default
/// starting with newer Windows releases) is the safe, dependency-free way
/// to read it.
#[cfg(target_os = "windows")]
pub(crate) fn boot_id() -> String {
    std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString('o')",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Informational only — never read by [`owner_is_gone`]'s liveness decision,
/// only shown back in `arbiter doctor`'s own output — so the portable
/// `hostname` command line utility already present on macOS and Windows
/// alike is simpler than chasing each OS's own API for a field this
/// unimportant.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// `/proc/<pid>` existing is the safe, dependency-free equivalent of `kill(pid,
/// 0)` on Linux.
#[cfg(target_os = "linux")]
fn pid_is_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// `kill -0 <pid>`'s exit status is the portable, `unsafe`-free equivalent of
/// `kill(pid, 0)` on macOS: success means the process exists and could be
/// signaled (same user or root); a clean "No such process" means it does not.
/// Any other failure — most commonly "Operation not permitted", a live
/// process owned by someone else — is deliberately read as alive: this
/// check's only job is telling a genuinely dead owner from everything else,
/// and failing closed (never stealable) is the safe direction to be wrong in
/// on anything ambiguous.
#[cfg(target_os = "macos")]
fn pid_is_alive(pid: u32) -> bool {
    match std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
    {
        Ok(o) if o.status.success() => true,
        Ok(o) => !String::from_utf8_lossy(&o.stderr).contains("No such process"),
        Err(_) => true,
    }
}

/// `Get-Process -Id` is the safe, `unsafe`-free equivalent on Windows of
/// Linux's `/proc/<pid>` existence check and macOS's `kill -0`: it looks the
/// process up by pid without sending it anything. Any failure to run
/// PowerShell itself is read as alive, for the same fail-closed reason as
/// the macOS check above.
#[cfg(target_os = "windows")]
fn pid_is_alive(pid: u32) -> bool {
    std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ 'ALIVE' }} else {{ 'DEAD' }}"),
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("ALIVE"))
        .unwrap_or(true)
}

/// INTERFACES §1's precondition table: gone when the boot differs (the recorded
/// pid refers to a previous boot and means nothing), or the boot matches and the
/// pid is not alive. `pub(crate)`: `blob::gc` (§8.2) must ask this exact same
/// question, not a simpler one — "`doctor` is a reader and cannot take a lease to
/// find out, so it must apply the *same* liveness predicate `reopen` uses."
pub(crate) fn owner_is_gone(
    recorded_boot_id: &str,
    recorded_pid: u32,
    current_boot_id: &str,
) -> bool {
    recorded_boot_id != current_boot_id || !pid_is_alive(recorded_pid)
}

#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("run already open")]
    AlreadyOpen,
    #[error("run not found")]
    NotFound,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// INTERFACES §1's `create`: the primary key does the work. A second `create`
/// loses with a constraint violation, mapped to `AlreadyOpen` — never blocking.
/// Returns the new `lease_epoch` (always 1) on success.
pub fn create(conn: &Connection, run_id: &str, owner: &Owner) -> Result<i64, LeaseError> {
    let now = crate::now_rfc3339();
    let result = conn.execute(
        "INSERT INTO run (run_id, owner_pid, boot_id, hostname, started_at, engine_version, lease_epoch)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
        rusqlite::params![run_id, owner.pid, owner.boot_id, owner.hostname, now, env!("CARGO_PKG_VERSION")],
    );
    match result {
        Ok(_) => Ok(1),
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(LeaseError::AlreadyOpen)
        }
        Err(e) => Err(LeaseError::Sqlite(e)),
    }
}

/// INTERFACES §1's `reopen`: read `(owner_pid, boot_id, lease_epoch)`, decide the
/// owner is gone, then compare-and-swap against the epoch just read.
/// `changes() == 1` means the lease is ours; `changes() == 0` means someone else
/// took it between the read and the write, mapped to `AlreadyOpen` — never a
/// liveness re-check, which is exactly the race this CAS exists to close.
pub fn reopen(conn: &Connection, run_id: &str, owner: &Owner) -> Result<i64, LeaseError> {
    let row: Option<(u32, String, i64)> = conn
        .query_row(
            "SELECT owner_pid, boot_id, lease_epoch FROM run WHERE run_id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (recorded_pid, recorded_boot_id, epoch) = row.ok_or(LeaseError::NotFound)?;

    if !owner_is_gone(&recorded_boot_id, recorded_pid, &owner.boot_id) {
        return Err(LeaseError::AlreadyOpen);
    }

    let now = crate::now_rfc3339();
    let changed = conn.execute(
        "UPDATE run SET owner_pid = ?1, boot_id = ?2, hostname = ?3, started_at = ?4, lease_epoch = lease_epoch + 1
         WHERE run_id = ?5 AND lease_epoch = ?6",
        rusqlite::params![owner.pid, owner.boot_id, owner.hostname, now, run_id, epoch],
    )?;

    if changed == 1 {
        Ok(epoch + 1)
    } else {
        Err(LeaseError::AlreadyOpen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::open_run_db;
    use std::sync::{Arc, Barrier};

    fn owner(pid: u32, boot_id: impl Into<String>) -> Owner {
        Owner {
            pid,
            boot_id: boot_id.into(),
            hostname: "test-host".to_string(),
        }
    }

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        open_run_db(&conn, "0.1.0", "2026-09-02T00:00:00Z").unwrap();
        conn
    }

    #[test]
    fn second_create_is_already_open() {
        let conn = fresh_db();
        let a = owner(std::process::id(), "boot-a");
        assert!(create(&conn, "run_1", &a).is_ok());

        let b = owner(std::process::id(), "boot-a");
        let result = create(&conn, "run_1", &b);
        assert!(matches!(result, Err(LeaseError::AlreadyOpen)));
    }

    #[test]
    fn stale_boot_id_is_stealable() {
        let conn = fresh_db();
        // "owned" by a process from a previous boot -- current_boot() differs.
        let dead = owner(999_999, "a-previous-boot");
        create(&conn, "run_1", &dead).unwrap();

        // reopen compares the *new* owner's real boot id against the recorded
        // (stale) one; use the process's own current boot id as the "new" owner.
        let real_current = owner(std::process::id(), super::boot_id());
        let epoch = reopen(&conn, "run_1", &real_current);
        assert_eq!(epoch.unwrap(), 2);
    }

    #[test]
    fn a_live_owner_cannot_be_stolen() {
        let conn = fresh_db();
        // Owned by *this* process, on *this* boot -- unambiguously alive.
        let live = owner(std::process::id(), super::boot_id());
        create(&conn, "run_1", &live).unwrap();

        let other = owner(std::process::id(), super::boot_id());
        let result = reopen(&conn, "run_1", &other);
        assert!(matches!(result, Err(LeaseError::AlreadyOpen)));
    }

    #[test]
    fn reopen_on_an_unknown_run_is_not_found() {
        let conn = fresh_db();
        let o = owner(std::process::id(), super::boot_id());
        assert!(matches!(
            reopen(&conn, "no_such_run", &o),
            Err(LeaseError::NotFound)
        ));
    }

    #[test]
    fn two_racing_reopens_only_one_wins() {
        // A file-backed db (not :memory:) so two real threads share one lease
        // table through independent connections, exactly like two processes
        // racing a real `arbiter run --resume`.
        let dir = std::env::temp_dir().join(format!("arbiter_lease_race_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("run.db");
        let _ = std::fs::remove_file(&db_path);

        {
            let conn = Connection::open(&db_path).unwrap();
            open_run_db(&conn, "0.1.0", "2026-09-02T00:00:00Z").unwrap();
            let dead = owner(999_999, "a-previous-boot");
            create(&conn, "run_1", &dead).unwrap();
        }

        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let db_path = db_path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let conn = Connection::open(&db_path).unwrap();
                conn.busy_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                let me = owner(std::process::id(), super::boot_id());
                barrier.wait(); // maximize the chance both read the same epoch
                reopen(&conn, "run_1", &me)
            }));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let wins = results.iter().filter(|r| r.is_ok()).count();
        let losses = results
            .iter()
            .filter(|r| matches!(r, Err(LeaseError::AlreadyOpen)))
            .count();
        assert_eq!(wins, 1, "exactly one racing reopen must win: {results:?}");
        assert_eq!(
            losses, 1,
            "the other must lose with AlreadyOpen: {results:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

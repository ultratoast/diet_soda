//! First-run initialization. Both `diet_soda --init` and the implicit
//! auto-initialization of the default config path call [`initialize`]; the
//! difference is only whether a status line is printed (and to which stream).
//!
//! Ordering and atomicity:
//!
//! 1. The parent directory and every sibling directory under it are created.
//! 2. Every companion file and prompt template is written with
//!    `OpenOptions::create_new(true)`, so an existing file is left alone.
//! 3. `config.json` is published last, via a same-directory temporary file
//!    plus `hard_link`. `hard_link` is an atomic create-if-absent on every
//!    supported platform: a non-cooperating writer that created
//!    `config.json` between the existence check and publication cannot be
//!    clobbered because the link call fails with `AlreadyExists` instead of
//!    replacing the destination.
//!
//! A crash anywhere before step 3 leaves the tree intact but no
//! `config.json`; the next launch self-heals because the existence check
//! returns false and `initialize` runs again. A crash anywhere after step 3
//! either committed the link or did not; the temporary file is cleaned up
//! on the error path. Companion files use `create_new(true)` so concurrent
//! auto-init calls cannot clobber each other.
//!
//! A persistent sentinel file (`~/.config/diet_soda/.diet_soda-init.lock`)
//! beside the config serializes concurrent first-run launches under the lock
//! so two processes cannot race to publish overlapping contents. The
//! sentinel itself is never removed; only the per-process `flock` (Unix) /
//! `LockFileEx` (Windows) lock it carries is held while the publisher is
//! alive. Contention surfaces as `WouldBlock` (Unix) or
//! `ERROR_LOCK_VIOLATION` (Windows); other errors are surfaced immediately.
use crate::{config::Config, tools};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    time::Duration,
};

/// Top-level companion files written beside `config.json`. Each entry is
/// `(relative_path, embedded_contents)`.
fn companion_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("AGENTS.md", include_str!("../examples/AGENTS.md")),
        ("theme.json", include_str!("../examples/theme.json")),
        ("bash-permissions.json", tools::DEFAULT_BASH_PERMISSIONS),
        (
            "CONFIGURATION.md",
            include_str!("../examples/CONFIGURATION.md"),
        ),
        (
            "QUEUE_AND_ACCESS.md",
            include_str!("../examples/QUEUE_AND_ACCESS.md"),
        ),
    ]
}

/// Prompt templates shipped beside the config so `./prompts/<name>.md`
/// references in the default agents resolve to real files.
fn prompt_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("plan.md", include_str!("../examples/prompts/plan.md")),
        ("build.md", include_str!("../examples/prompts/build.md")),
        (
            "code-review.md",
            include_str!("../examples/prompts/code-review.md"),
        ),
        (
            "plan-review.md",
            include_str!("../examples/prompts/plan-review.md"),
        ),
        ("debug.md", include_str!("../examples/prompts/debug.md")),
        (
            "research.md",
            include_str!("../examples/prompts/research.md"),
        ),
        ("explore.md", include_str!("../examples/prompts/explore.md")),
        (
            "test-runner.md",
            include_str!("../examples/prompts/test-runner.md"),
        ),
        (
            "test-writer.md",
            include_str!("../examples/prompts/test-writer.md"),
        ),
        (
            "general-purpose.md",
            include_str!("../examples/prompts/general-purpose.md"),
        ),
        (
            "converse.md",
            include_str!("../examples/prompts/converse.md"),
        ),
        (
            "elephant.md",
            include_str!("../examples/prompts/elephant.md"),
        ),
    ]
}

/// Default directories created beside `config.json`. Created with
/// `create_dir_all` so they are always present even when an existing tree
/// already had one or more of them.
fn companion_dirs() -> Vec<&'static str> {
    vec!["workflows", "skills", "prompts", "sessions", "exports"]
}

/// Sentinel file used to serialize concurrent first-run launches. The
/// sentinel itself persists across launches; only the per-process lock on
/// it is released when the returned guard is dropped.
const LOCK_NAME: &str = ".diet_soda-init.lock";

/// Windows `LockFileEx` reports contention as raw OS error 33
/// (`ERROR_LOCK_VIOLATION`); `fs2` propagates that directly. Unix `flock`
/// already reports contention as `ErrorKind::WouldBlock`.
#[cfg(windows)]
fn is_lock_contention(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(33)
}

#[cfg(not(windows))]
fn is_lock_contention(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
}

/// Write `contents` to `target`, refusing to touch an existing file. Used for
/// every companion file so concurrent auto-init calls cannot overwrite.
fn write_if_absent(target: &Path, contents: &str) -> Result<()> {
    match OpenOptions::new().write(true).create_new(true).open(target) {
        Ok(mut file) => {
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| format!("Writing {}", target.display())),
    }
}

/// Build the JSON document used as the initial config. Shared by both
/// `--init` and the implicit auto-init path so the two entry points publish
/// the same tree (default agents, `system_prompt`, and theme references).
fn default_document() -> Result<serde_json::Value> {
    let mut document = serde_json::to_value(Config::default())?;
    document["agents"] = crate::config::default_agent_entries();
    document["system_prompt"] = serde_json::json!("./AGENTS.md");
    document["theme"] = serde_json::json!("./theme.json");
    Ok(document)
}

/// Render the default config document to a stable UTF-8 byte vector.
fn render_config_bytes(document: &serde_json::Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(document)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Run `body` while holding an exclusive lock on the persistent sentinel at
/// `lock_path`. Blocks (with short retries) when another process holds the
/// lock. The lock is released when the returned guard is dropped at the end
/// of the call; the sentinel file itself stays on disk for the next launch.
fn with_init_lock<F>(lock_path: &Path, body: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    fs::create_dir_all(
        lock_path
            .parent()
            .context("Lock path has no parent directory")?,
    )?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(lock_path)
        .with_context(|| format!("Opening init lock {}", lock_path.display()))?;
    // Bounded wait before treating contention as a fatal startup failure.
    // `fs2` flock is per-process; an orphan lock only persists while the
    // previous process is alive, so this is purely about giving a slow but
    // legitimate publisher time to finish. Two scenarios push the wait well
    // above a couple of seconds in practice:
    //   * Windows. `LockFileEx` contention here has surfaced as multi-second
    //     waits during Defender-real-time scans of the freshly-written
    //     `config.json` and prompt templates; `sync_all` blocks until the
    //     scanner finishes.
    //   * Networked/AV-heavy filesystems (SMB/NFS/OneDrive, journaling FSs
    //     with eager flushing). A handful of `fsync` calls plus a couple of
    //     `hard_link` round-trips can stretch to tens of seconds while the
    //     journal/AV pipeline catches up.
    // Thirty seconds is comfortably above the longest observed legitimate
    // publish yet orders of magnitude below a hung process, so a true crash
    // (where the OS has reclaimed the lock) still self-heals within seconds.
    let mut waited = Duration::from_millis(0);
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => break,
            Err(error) if is_lock_contention(&error) => {
                if waited >= Duration::from_secs(30) {
                    return Err(error).with_context(|| {
                        format!(
                            "Another diet_soda process is initializing {}",
                            lock_path.display()
                        )
                    });
                }
                std::thread::sleep(Duration::from_millis(50));
                waited += Duration::from_millis(50);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Locking {}", lock_path.display()))
            }
        }
    }
    body()
}

/// Public entry point shared by `--init` and the implicit auto-init path.
///
/// `path` is the location of `config.json`. A pre-existing file at that path
/// causes a clean `already exists` error so callers can map it to the
/// desired user-facing message. Concurrent callers serialize on a lock file
/// beside the config; the loser sees a fully-published tree and proceeds.
pub fn initialize(path: &Path) -> Result<()> {
    let directory = path
        .parent()
        .context("Config path has no parent directory")?;
    fs::create_dir_all(directory)
        .with_context(|| format!("Creating config directory {}", directory.display()))?;
    let lock_path = directory.join(LOCK_NAME);
    with_init_lock(&lock_path, || initialize_locked(path, directory))
}

fn initialize_locked(path: &Path, directory: &Path) -> Result<()> {
    if path.exists() {
        bail!(
            "{} already exists; existing files are never overwritten",
            path.display()
        );
    }
    // Sibling directories first. `create_dir_all` is idempotent.
    for name in companion_dirs() {
        fs::create_dir_all(directory.join(name))
            .with_context(|| format!("Creating directory {name}"))?;
    }
    // Companion and prompt files next, all with `create_new(true)` so any
    // existing file (left behind by a prior partial run, or owned by a
    // non-cooperating writer) is left alone. Writing these before
    // `config.json` is what makes a crash before publication self-heal: on
    // the next run, `path.exists()` returns false, init runs again, and the
    // existing companion files are detected as already present.
    for (name, contents) in companion_files() {
        write_if_absent(&directory.join(name), contents)?;
    }
    for (name, contents) in prompt_files() {
        write_if_absent(&directory.join("prompts").join(name), contents)?;
    }
    // Publish `config.json` last. Any observer that sees the file sees a
    // complete tree; the lock above guarantees no cooperating publisher
    // runs concurrently, and `hard_link` is an atomic create-if-absent on
    // every supported platform so a non-cooperating writer that appeared
    // between the `path.exists()` check and now cannot be clobbered (the
    // link would fail with `AlreadyExists`). `publish_config` cleans up the
    // staging file on every exit path so we never leave a `.tmp` behind.
    let temp = path.with_file_name(format!(".config-{}.tmp", uuid::Uuid::new_v4()));
    publish_config(path, &temp)
}

/// Stage `config.json` to `temp`, then publish it via `hard_link` followed
/// by removing the staging file. `hard_link` fails with `AlreadyExists` if
/// `path` is already present, so a non-cooperating writer that appeared
/// between the upfront existence check and publication is preserved instead
/// of overwritten. Mapping that case to the same friendly error that the
/// upfront check uses keeps `--init` semantics stable.
fn publish_config(path: &Path, temp: &Path) -> Result<()> {
    let document = default_document()?;
    let bytes = render_config_bytes(&document)?;
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temp)
            .with_context(|| format!("Creating temporary {}", temp.display()))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    // Atomic create-if-absent publication. `fs::rename` would overwrite
    // the destination on every supported platform; `hard_link` shares the
    // inode and fails with `AlreadyExists` if the destination is already
    // present, so a non-cooperating writer cannot be clobbered. The source
    // and destination are in the same directory so the link is always
    // legal; we never need to fall back to copy.
    let link_result = fs::hard_link(temp, path);
    // The staging file is no longer needed once publication has been
    // attempted. Removing it before propagating the error keeps a failed
    // publish from leaving a `.tmp` file in the user's config directory.
    let _ = fs::remove_file(temp);
    match link_result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => bail!(
            "{} already exists; existing files are never overwritten",
            path.display()
        ),
        Err(error) => Err(error)
            .with_context(|| format!("Linking temporary {} to {}", temp.display(), path.display())),
    }
}

/// Implicit auto-init used when no explicit `--config` was supplied and the
/// default path is missing. Status is written to `sink` so callers can
/// route it to stdout (for `--init`) or stderr (for the implicit auto-init
/// path).
pub fn auto_initialize<W: Write>(path: &Path, sink: &mut W) -> Result<()> {
    // Re-check existence without holding the lock so a fast path stays cheap
    // when a previous run already published the file.
    if path.exists() {
        return Ok(());
    }
    match initialize(path) {
        Ok(()) => {
            // Initialization has already succeeded. The status line is a
            // best-effort notification; if the sink (typically stderr) is
            // closed or unwritable, swallow that specific failure so the
            // caller still proceeds to load the freshly-published config.
            if let Err(error) = writeln!(
                sink,
                "Initialized default configuration at {}",
                path.display()
            ) {
                if error.kind() == std::io::ErrorKind::BrokenPipe {
                    return Ok(());
                }
                return Err(error.into());
            }
            Ok(())
        }
        Err(error) => {
            // Another process can win the race between the existence check
            // and the lock acquisition; treat that as a successful no-op so
            // we proceed to load the freshly-published file.
            if path.exists() {
                Ok(())
            } else {
                Err(error).with_context(|| {
                    format!(
                        "Automatic initialization failed for {}; run `diet_soda --init` to investigate",
                        path.display()
                    )
                })
            }
        }
    }
}

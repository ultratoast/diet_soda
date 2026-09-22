//! Filesystem helpers that apply owner-only permissions to newly created
//! paths without touching paths that already exist.
//!
//! New-creation-only semantics: these helpers affect only *new* Unix inodes.
//! An existing directory or file is never chmodded, so a caller cannot widen
//! or narrow permissions an operator set deliberately. The requested modes
//! are also masked by the process umask, so the effective mode can be
//! narrower than requested. On non-Unix platforms the helpers fall back to
//! the ordinary `std::fs` calls, which is the platform-appropriate behavior.

use std::fs::OpenOptions;
use std::io;
use std::path::Path;

/// Recursively create a directory, giving every newly created Unix directory
/// mode `0o700`.
///
/// Existing directories are left untouched; this function never changes their
/// permissions. On non-Unix platforms this is plain [`std::fs::create_dir_all`].
pub fn create_dir_all_private(path: impl AsRef<Path>) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

/// [`OpenOptions`] preconfigured to give a newly created Unix file mode
/// `0o600`.
///
/// Callers add the `create`/`create_new`/`write`/`append` flags they need. The
/// mode applies only when the file is created; opening an existing file does
/// not change its permissions. On non-Unix platforms this is a plain
/// [`OpenOptions::new`].
pub fn private_open_options() -> OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options.mode(0o600);
        options
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new()
    }
}

/// Harden an already extracted tree for private use by its owner.
///
/// This walks an *existing* tree without following symbolic links. On Unix
/// every directory is chmodded to mode `0o700` and every regular file to mode
/// `0o600`; file contents are never modified. Symbolic links and special
/// entries (anything that is neither a directory nor a regular file) are
/// rejected with an error rather than silently skipped.
///
/// Unlike [`create_dir_all_private`] and [`private_open_options`], which only
/// affect newly created inodes, this helper chmods inodes that already exist.
/// It is intended solely for a freshly extracted staging tree whose ownership
/// and layout are already trusted; it does not sanitize arbitrary existing
/// trees. On non-Unix platforms the tree is traversed and validated without
/// changing any permissions.
pub fn harden_tree_private(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    let file_type = std::fs::symlink_metadata(path)?.file_type();

    if file_type.is_symlink() {
        return Err(invalid_entry("symbolic link", path));
    }
    if file_type.is_dir() {
        // Recurse into children before tightening the directory itself, so a
        // read failure fails loudly without having already locked the parent.
        for entry in std::fs::read_dir(path)? {
            harden_tree_private(entry?.path())?;
        }
        set_private_mode(path, 0o700)
    } else if file_type.is_file() {
        set_private_mode(path, 0o600)
    } else {
        Err(invalid_entry("special entry", path))
    }
}

fn invalid_entry(kind: &str, path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("refusing to harden {}: {}", kind, path.display()),
    )
}

#[cfg(unix)]
fn set_private_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_private_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

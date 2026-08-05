// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Ownership checks for the daemon's filesystem surface and its socket peers.
//!
//! The daemon socket carries unauthenticated command access to a whole graph —
//! including `ingest_archive` and `shutdown` — so both ends verify that the
//! other side runs as the same uid, and every directory the socket lives in
//! must be owner-only.
//!
//! Everything here **fails closed**: when ownership cannot be established the
//! operation is refused with a named [`RecallError::Daemon`], never assumed
//! safe. In particular the directory checks never *repair* a directory they
//! did not create — chmod-ing a pre-existing directory is how a local user
//! turns a symlink into a privilege escalation.

use std::fs::DirBuilder;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::error::RecallError;

/// Mode of every directory recall-echo creates at runtime: owner-only.
pub(crate) const PRIVATE_DIR_MODE: u32 = 0o700;
/// Mode of every runtime file recall-echo creates: owner-only.
pub(crate) const PRIVATE_FILE_MODE: u32 = 0o600;

/// Permission bits that must never be set on a directory recall-echo derives
/// and creates itself.
const FOREIGN_ACCESS: u32 = 0o077;
/// Permission bits that let another user drop a socket of their own into a
/// directory — the bits that actually enable a socket hijack.
const FOREIGN_WRITE: u32 = 0o022;

/// The uid recall-echo runs as.
///
/// Read from the filesystem (`/proc/self`, then the home directory) so no libc
/// dependency is needed. An unknown uid is an error — never a silent `0`,
/// which would make every ownership comparison pass for root-owned paths.
pub(crate) fn current_uid() -> Result<u32, RecallError> {
    std::fs::metadata("/proc/self")
        .or_else(|_| {
            let home = dirs::home_dir()
                .ok_or_else(|| std::io::Error::new(ErrorKind::NotFound, "no home directory"))?;
            std::fs::metadata(home)
        })
        .map(|meta| meta.uid())
        .map_err(|err| {
            RecallError::Daemon(format!(
                "cannot determine the uid recall-echo runs as ({err}); \
                 refusing to use the graph daemon socket"
            ))
        })
}

/// Options that create a file owner-only, failing if it already exists.
pub(crate) fn create_new_private_file() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true).mode(PRIVATE_FILE_MODE);
    options
}

/// Options that append to a file, creating it owner-only.
pub(crate) fn append_private_file() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true).mode(PRIVATE_FILE_MODE);
    options
}

/// Create `dir` (and any missing parents) owner-only, then verify it.
///
/// Only for directories recall-echo derives itself. Each component is created
/// with mode `0o700` by the kernel, so there is no window where the directory
/// exists with wider permissions; a component that already exists is validated,
/// never chmod-ed.
pub(crate) fn create_private_dir(dir: &Path) -> Result<(), RecallError> {
    if let Some(parent) = dir.parent() {
        require_safe_container(parent)?;
    }
    DirBuilder::new()
        .mode(PRIVATE_DIR_MODE)
        .recursive(true)
        .create(dir)
        .map_err(|err| {
            RecallError::Daemon(format!(
                "cannot create daemon socket directory {}: {err}",
                dir.display()
            ))
        })?;
    require_private_dir(dir)
}

/// Verify that `dir` is an owner-only directory belonging to this user.
///
/// The strict policy, for the runtime directory recall-echo derives and
/// creates itself: anything looser than `0o700` there means somebody else
/// created it first.
pub(crate) fn require_private_dir(dir: &Path) -> Result<(), RecallError> {
    let meta = inspect_dir(dir)?;
    let mode = meta.permissions().mode();
    if mode & FOREIGN_ACCESS != 0 {
        return Err(refuse(
            dir,
            &format!(
                "is accessible to other users (mode {:04o}, needs {PRIVATE_DIR_MODE:04o})",
                mode & 0o7777
            ),
        ));
    }
    Ok(())
}

/// Verify that `dir` is a directory only this user can write into.
///
/// The policy for a directory recall-echo does *not* own — one named by
/// `[serve] socket_path`, which comes from a config file an untrusted checkout
/// can supply. Such a directory is never created and never chmod-ed, only
/// accepted or refused, and a conventional `0o755` home-relative directory is
/// perfectly safe: what matters is that no other user can put their own socket
/// there for us to talk to.
pub(crate) fn require_owned_dir(dir: &Path) -> Result<(), RecallError> {
    let meta = inspect_dir(dir)?;
    let mode = meta.permissions().mode();
    if mode & FOREIGN_WRITE != 0 && mode & 0o1000 == 0 {
        return Err(refuse(
            dir,
            &format!("is writable by other users (mode {:04o})", mode & 0o7777),
        ));
    }
    Ok(())
}

/// Shared checks: a real directory, not a symlink, owned by this user.
fn inspect_dir(dir: &Path) -> Result<std::fs::Metadata, RecallError> {
    let meta = std::fs::symlink_metadata(dir).map_err(|err| {
        RecallError::Daemon(format!(
            "cannot use daemon socket directory {}: {err}. \
             Create it, or set `[serve] socket_path` in .recall-echo.toml.",
            dir.display()
        ))
    })?;

    if meta.file_type().is_symlink() {
        return Err(refuse(dir, "is a symlink"));
    }
    if !meta.is_dir() {
        return Err(refuse(dir, "is not a directory"));
    }

    let owner = current_uid()?;
    if meta.uid() != owner {
        return Err(refuse(
            dir,
            &format!("is owned by uid {}, not {owner}", meta.uid()),
        ));
    }
    Ok(meta)
}

fn refuse(dir: &Path, reason: &str) -> RecallError {
    RecallError::Daemon(format!(
        "refusing to use daemon socket directory {}: it {reason}. \
         Remove it, or set `[serve] socket_path` in .recall-echo.toml to a directory you own.",
        dir.display()
    ))
}

/// Verify that a directory recall-echo is about to create something inside is
/// not a hijack point: it must belong to this user or to root, and if it is
/// group- or world-writable it must carry the sticky bit (as `/tmp` does).
/// Without that, another user could rename our socket directory away and put
/// theirs in its place.
fn require_safe_container(parent: &Path) -> Result<(), RecallError> {
    let Ok(meta) = std::fs::metadata(parent) else {
        // Missing or unreadable: the create call below reports it precisely.
        return Ok(());
    };
    let mode = meta.permissions().mode();
    let shared_writable = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    let owner = current_uid()?;

    if meta.uid() != owner && meta.uid() != 0 {
        return Err(refuse(
            parent,
            &format!(
                "holds our socket directory but is owned by uid {}",
                meta.uid()
            ),
        ));
    }
    if shared_writable && !sticky {
        return Err(refuse(
            parent,
            &format!(
                "holds our socket directory but is writable by other users without the sticky bit (mode {:04o})",
                mode & 0o7777
            ),
        ));
    }
    Ok(())
}

/// Remove a unix socket, refusing to unlink anything else.
///
/// `bind` reports `EADDRINUSE` for a *regular* file too, so an unguarded
/// "clear the stale socket" step turns a config-supplied `socket_path` into an
/// arbitrary-file delete. Missing paths are not an error — the socket being
/// gone is the desired end state.
pub(crate) fn unlink_socket(path: &Path) -> Result<(), RecallError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(RecallError::Daemon(format!(
                "cannot inspect the daemon socket {}: {err}",
                path.display()
            )))
        }
    };

    if !meta.file_type().is_socket() {
        return Err(RecallError::Daemon(format!(
            "refusing to remove {}: it is not a unix socket. \
             Check `[serve] socket_path` in .recall-echo.toml.",
            path.display()
        )));
    }
    let owner = current_uid()?;
    if meta.uid() != owner {
        return Err(RecallError::Daemon(format!(
            "refusing to remove the daemon socket {}: it is owned by uid {}, not {owner}",
            path.display(),
            meta.uid()
        )));
    }

    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(RecallError::Daemon(format!(
            "cannot remove the daemon socket {}: {err}",
            path.display()
        ))),
    }
}

/// Accept a socket peer only when it runs as the daemon's own uid.
///
/// The unix socket has no other authentication: whoever connects can read
/// every ingest payload and forge every query result.
pub(crate) fn check_peer_uid(peer_uid: u32, owner_uid: u32) -> Result<(), RecallError> {
    if peer_uid == owner_uid {
        return Ok(());
    }
    Err(RecallError::Daemon(format!(
        "graph daemon socket peer uid {peer_uid} does not match the owner uid {owner_uid}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn current_uid_matches_a_file_we_just_created() {
        let dir = tempfile::tempdir().unwrap();
        let meta = std::fs::metadata(dir.path()).unwrap();
        assert_eq!(current_uid().unwrap(), meta.uid());
    }

    #[test]
    fn created_socket_dir_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let socket_dir = dir.path().join("run");
        create_private_dir(&socket_dir).unwrap();

        let mode = std::fs::metadata(&socket_dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, PRIVATE_DIR_MODE);
    }

    #[test]
    fn nested_socket_dirs_are_each_created_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        create_private_dir(&nested).unwrap();

        for path in [dir.path().join("a"), nested] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, PRIVATE_DIR_MODE, "{}", path.display());
        }
    }

    /// The hijack: a local user pre-creates the socket directory world-writable
    /// so they can drop their own socket in it.
    #[test]
    fn a_world_writable_socket_dir_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let hijacked = dir.path().join("run");
        std::fs::create_dir(&hijacked).unwrap();
        std::fs::set_permissions(&hijacked, std::fs::Permissions::from_mode(0o777)).unwrap();

        for err in [
            create_private_dir(&hijacked).unwrap_err(),
            require_private_dir(&hijacked).unwrap_err(),
            require_owned_dir(&hijacked).unwrap_err(),
        ] {
            assert!(matches!(err, RecallError::Daemon(_)), "{err}");
        }

        // The refusal must not have "repaired" the directory behind our back —
        // chmod-ing a directory we did not create is the escalation itself.
        let mode = std::fs::metadata(&hijacked).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o777);
    }

    /// A conventional `0o755` directory cannot be hijacked (nobody else can
    /// write in it), so a configured `socket_path` may live there — but the
    /// runtime directory recall-echo creates itself must still be `0o700`.
    #[test]
    fn a_readable_but_unwritable_socket_dir_is_owner_only_enough() {
        let dir = tempfile::tempdir().unwrap();
        let conventional = dir.path().join("run");
        std::fs::create_dir(&conventional).unwrap();
        std::fs::set_permissions(&conventional, std::fs::Permissions::from_mode(0o755)).unwrap();

        require_owned_dir(&conventional).unwrap();

        let err = require_private_dir(&conventional).unwrap_err();
        assert!(
            err.to_string().contains("accessible to other users"),
            "{err}"
        );
    }

    #[test]
    fn a_symlinked_socket_dir_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("run");
        symlink(&target, &link).unwrap();

        for err in [
            require_private_dir(&link).unwrap_err(),
            require_owned_dir(&link).unwrap_err(),
            // create_dir_all follows symlinks, so creating must refuse too.
            create_private_dir(&link).unwrap_err(),
        ] {
            assert!(err.to_string().contains("is a symlink"), "{err}");
        }
    }

    #[test]
    fn a_regular_file_is_not_a_socket_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("run");
        std::fs::write(&file, b"x").unwrap();

        let err = require_owned_dir(&file).unwrap_err();
        assert!(err.to_string().contains("socket directory"), "{err}");
        assert!(err.to_string().contains("is not a directory"), "{err}");
    }

    #[test]
    fn a_missing_socket_dir_is_a_named_daemon_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = require_owned_dir(&dir.path().join("nope")).unwrap_err();
        assert!(matches!(err, RecallError::Daemon(_)), "{err}");
        assert!(err.to_string().contains("socket directory"), "{err}");
    }

    #[test]
    fn unlink_socket_removes_only_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("g.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        drop(listener);

        unlink_socket(&socket).unwrap();
        assert!(!socket.exists());
        // Idempotent: a missing socket is the desired end state.
        unlink_socket(&socket).unwrap();
    }

    #[test]
    fn unlink_socket_refuses_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("precious.toml");
        std::fs::write(&file, b"keep me").unwrap();

        let err = unlink_socket(&file).unwrap_err();
        assert!(err.to_string().contains("not a unix socket"), "{err}");
        assert!(file.exists(), "the file must survive the refusal");
    }

    #[test]
    fn unlink_socket_refuses_a_symlink_to_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("g.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let link = dir.path().join("link.sock");
        symlink(&socket, &link).unwrap();

        let err = unlink_socket(&link).unwrap_err();
        assert!(err.to_string().contains("not a unix socket"), "{err}");
        assert!(socket.exists(), "the target must survive");
        drop(listener);
    }

    #[test]
    fn peer_uid_must_match_the_owner() {
        assert!(check_peer_uid(1000, 1000).is_ok());

        let err = check_peer_uid(1001, 1000).unwrap_err();
        assert!(matches!(err, RecallError::Daemon(_)), "{err}");
        assert!(err.to_string().contains("1001"), "{err}");
        assert!(err.to_string().contains("1000"), "{err}");
    }

    #[test]
    fn private_file_options_create_owner_only_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");

        drop(append_private_file().open(&path).unwrap());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, PRIVATE_FILE_MODE);

        let exclusive = dir.path().join("g.sock.lock");
        drop(create_new_private_file().open(&exclusive).unwrap());
        assert!(create_new_private_file().open(&exclusive).is_err());
        let mode = std::fs::metadata(&exclusive).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, PRIVATE_FILE_MODE);
    }
}

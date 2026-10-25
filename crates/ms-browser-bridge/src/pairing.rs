/*
File: crates/ms-browser-bridge/src/pairing.rs

Purpose:
The pairing file `bridge.json` in the unpacked extension folder and the folder's inter-process
owner lock: take the lock, read the previous file, keep its secret, bind the loopback listener on
its port when possible (else any free port), write the result back atomically and make sure the
final file is private. The extension re-reads this file before every connect.

Key items:
- `bind_paired(dir)` -> `(TcpListener, Pairing, OwnerLock)`.
- `OwnerLock` / `acquire_owner_lock(dir)` — `.ms-bridge.lock`, one server per folder across
  processes.
- `read_bridge_file(path)` / `write_bridge_file(path, file)`.

Notes:
- Owner lock: an exclusive advisory lock (`File::try_lock`) on `OWNER_LOCK_FILE_NAME` in the
  folder, taken BEFORE the file is read and held by the caller for the listener's whole life, so
  read -> secret choice -> bind -> write and the served pairing belong to one process. A second
  app process gets `BridgeError::OwnedByOtherProcess` and starts no server (it would otherwise
  rewrite `bridge.json` and split the extension from the first one). The OS drops the lock when
  the owner process dies, so no stale-lock cleanup exists; the lock file itself is never deleted.
- The secret is created once and reused for as long as the file holds a well-formed one that is
  TRUSTED (unix: a regular file owned by the current user; elsewhere: a regular file): an
  already-loaded extension keeps working across app restarts. A malformed, foreign-owned or
  non-regular file (symlink) is replaced with a fresh secret (it is the bridge's own file, never a
  user document).
- The file is rewritten only when its content changes (port fallback, new secret, protocol).
- Writes: `ms_atomic_file::write_atomic_with` (temp `.bridge.json.{pid}.tmp` in the same folder,
  created mode 0600 on unix and also forced on a reused temp before the secret is written ->
  fsync -> rename over the target). Every start then checks the FINAL file on unix: owned by the current
  user, mode 0600 (a wider mode left by a copy / restore is repaired), else `PairingIo`.
- Never log the secret or the file content.
*/

use std::fs::{self, File, TryLockError};
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::path::{Path, PathBuf};

use ms_atomic_file::{AtomicWriteOptions, Durability, write_atomic_with};
use ms_log::runtime_log;

use crate::auth::Secret;
use crate::errors::BridgeError;
use crate::protocol::{BridgeFile, CONFIG_FILE_NAME, PROTOCOL_VERSION};

/// File name of the folder's inter-process owner lock (see the file header). A dot file: never
/// part of the bundle, ignored by the browser.
pub(crate) const OWNER_LOCK_FILE_NAME: &str = ".ms-bridge.lock";

/// Exclusive inter-process ownership of one extension folder's bridge. Released when dropped (and
/// by the OS when the process dies). The server keeps it alive exactly as long as its listener.
#[derive(Debug)]
pub(crate) struct OwnerLock {
    /// The locked file; the lock lives as long as this handle.
    file: File,
}

impl Drop for OwnerLock {
    fn drop(&mut self) {
        // Closing the handle releases the lock too; the explicit unlock only makes the release
        // independent of when the OS closes the descriptor. A failure is harmless for that reason.
        if let Err(err) = self.file.unlock() {
            runtime_log::log_info(format!("[browser_bridge] owner lock release deferred to close: {err}"));
        }
    }
}

/// Takes the owner lock of `dir` (created if missing) without waiting.
///
/// # Errors
/// `OwnedByOtherProcess` when another live owner holds it (another app process, or another
/// server of this process on the same folder); `PairingIo` when the folder or the lock file cannot
/// be created / opened or the lock call itself fails.
pub(crate) fn acquire_owner_lock(dir: &Path) -> Result<OwnerLock, BridgeError> {
    fs::create_dir_all(dir).map_err(|err| pairing_error(dir, &err))?;
    let path = dir.join(OWNER_LOCK_FILE_NAME);
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    // Only the current user may open (and so lock) it: another account cannot block the bridge.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).map_err(|err| pairing_error(&path, &err))?;
    match file.try_lock() {
        Ok(()) => Ok(OwnerLock { file }),
        Err(TryLockError::WouldBlock) => {
            runtime_log::log_warn(format!("[browser_bridge] {} is held by another running app; this process starts no bridge server", path.display()));
            Err(BridgeError::OwnedByOtherProcess)
        }
        Err(TryLockError::Error(err)) => Err(pairing_error(&path, &err)),
    }
}

/// The live pairing of a started server.
#[derive(Debug, Clone)]
pub(crate) struct Pairing {
    /// The bound loopback port.
    pub(crate) port: u16,
    /// The shared secret.
    pub(crate) secret: Secret,
}

/// Reads `path` as a `BridgeFile`. `Ok(None)` when the file does not exist or does not parse
/// (logged; the caller then writes a fresh one).
///
/// # Errors
/// `BridgeError::PairingIo` on any other read failure (permissions, I/O).
pub(crate) fn read_bridge_file(path: &Path) -> Result<Option<BridgeFile>, BridgeError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(pairing_error(path, &err)),
    };
    match serde_json::from_slice::<BridgeFile>(&bytes) {
        Ok(file) => Ok(Some(file)),
        Err(err) => {
            // The parse error names a position, never the content.
            runtime_log::log_warn(format!("[browser_bridge] {} is malformed and will be rewritten: {err}", path.display()));
            Ok(None)
        }
    }
}

/// Writes `file` to `path` atomically through the workspace's one recipe
/// (`ms_atomic_file::write_atomic_with`): the folder is created when missing, the temp file in
/// the same folder is created with mode 0600 on unix (also forced on a reused temp, before the
/// secret is written), fsynced and renamed over `path`. The rename always replaces the
/// directory entry (no identical-bytes shortcut): `bind_paired` calls this only when the
/// content changed or the old entry is untrusted (a symlink, a foreign owner), and such an
/// entry must never be kept.
///
/// # Errors
/// `BridgeError::PairingIo` when the folder cannot be created or any write step fails; the
/// temp file is removed on failure (best effort, by the recipe).
pub(crate) fn write_bridge_file(path: &Path, file: &BridgeFile) -> Result<(), BridgeError> {
    let bytes = serde_json::to_vec_pretty(file).map_err(|err| BridgeError::PairingIo { path: path.to_path_buf(), detail: err.to_string() })?;
    // The secret is readable by the current user only (the browser runs as that user).
    let options = AtomicWriteOptions { create_parent: true, unix_mode: Some(0o600), skip_if_identical: false, ..AtomicWriteOptions::new(Durability::Contents) };
    // The recipe's error names the directory, temp file or target it failed on.
    write_atomic_with(path, &bytes, &options).map_err(|err| BridgeError::PairingIo { path: path.to_path_buf(), detail: err.to_string() })
}

fn pairing_error(path: &Path, err: &io::Error) -> BridgeError {
    BridgeError::PairingIo { path: path.to_path_buf(), detail: err.to_string() }
}

/// The `bridge.json` path inside the unpacked extension folder `dir`.
#[must_use]
pub(crate) fn bridge_file_path(dir: &Path) -> PathBuf {
    dir.join(CONFIG_FILE_NAME)
}

/// Takes the folder's owner lock, binds the loopback listener and settles the pairing file in
/// `dir`.
///
/// Reuses the previous secret when the file is trusted and well-formed (else generates one) and
/// the previous port when it can be bound again (else `127.0.0.1:0`), rewrites `bridge.json` if
/// anything changed, then makes sure the final file is private (unix). The returned `OwnerLock`
/// must live as long as the listener. Blocking file I/O: worker threads only.
///
/// # Errors
/// `OwnedByOtherProcess` (another live owner of the folder; nothing was read or written),
/// `PairingIo` (lock, read, write or permission check of the file), `Bind` (no loopback port),
/// `System` (OS random source).
pub(crate) fn bind_paired(dir: &Path) -> Result<(TcpListener, Pairing, OwnerLock), BridgeError> {
    let owner = acquire_owner_lock(dir)?;
    let path = bridge_file_path(dir);
    let previous = match untrusted_reason(&path)? {
        None => read_bridge_file(&path)?,
        Some(reason) => {
            runtime_log::log_warn(format!("[browser_bridge] {} is {reason}; its secret is not reused and the file is replaced", path.display()));
            None
        }
    };
    let secret = match previous.as_ref().and_then(|file| Secret::from_hex(&file.secret)) {
        Some(secret) => secret,
        None => Secret::generate().map_err(|err| BridgeError::System { detail: err.to_string() })?,
    };
    let listener = bind_loopback(previous.as_ref().map(|file| file.port))?;
    let port = listener.local_addr().map_err(|err| BridgeError::Bind { detail: err.to_string() })?.port();
    let current = BridgeFile { protocol: PROTOCOL_VERSION, port, secret: secret.to_hex() };
    if previous.as_ref() != Some(&current) {
        write_bridge_file(&path, &current)?;
        runtime_log::log_info(format!("[browser_bridge] pairing file written: {} (port {port})", path.display()));
    }
    ensure_private(&path)?;
    Ok((listener, Pairing { port, secret }, owner))
}

/// Why the existing `path` must not be trusted (its secret not reused), or `None` when it is
/// trusted or absent.
///
/// # Errors
/// `PairingIo` when its metadata cannot be read for another reason than absence.
fn untrusted_reason(path: &Path) -> Result<Option<&'static str>, BridgeError> {
    // `symlink_metadata`: a symlink is judged as itself, never through its target.
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(pairing_error(path, &err)),
    };
    Ok(trust_verdict(meta.file_type().is_file(), owned_by_current_user(&meta)))
}

/// The trust rule of an existing pairing file: a regular file, owned by the current user
/// (`owned_by_current_user`, see [`owned_by_current_user`]). `None` = trusted.
fn trust_verdict(is_regular_file: bool, owned_by_current_user: bool) -> Option<&'static str> {
    if !is_regular_file {
        return Some("not a regular file");
    }
    if !owned_by_current_user {
        return Some("owned by another user");
    }
    None
}

/// Unix: the file's owner is the process's effective user.
#[cfg(unix)]
fn owned_by_current_user(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: `geteuid` has no preconditions, touches no memory of ours and cannot fail (POSIX).
    let current = unsafe { libc::geteuid() };
    meta.uid() == current
}

/// Elsewhere there is no owner id in the metadata: the folder's ACL (the user's data directory)
/// is the boundary, so every file counts as the user's.
#[cfg(not(unix))]
fn owned_by_current_user(_meta: &fs::Metadata) -> bool {
    true
}

/// Unix: checks that the final `path` is a regular file of the current user and forces mode
/// 0600 when it is wider or narrower (a reused file may come from a copy / restore). Elsewhere a
/// no-op: the folder's ACL (the user's data directory) is the boundary.
///
/// # Errors
/// `PairingIo` when the file is not a regular file of the current user (a race replaced it after
/// the write) or its mode cannot be read / set.
#[cfg(unix)]
fn ensure_private(path: &Path) -> Result<(), BridgeError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(path).map_err(|err| pairing_error(path, &err))?;
    if let Some(reason) = trust_verdict(meta.file_type().is_file(), owned_by_current_user(&meta)) {
        return Err(BridgeError::PairingIo { path: path.to_path_buf(), detail: format!("the pairing file is {reason}") });
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0o600 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|err| pairing_error(path, &err))?;
        runtime_log::log_warn(format!("[browser_bridge] {} had mode {mode:o}; restricted to 600", path.display()));
    }
    Ok(())
}

#[cfg(not(unix))]
#[expect(clippy::unnecessary_wraps, reason = "same signature as the unix variant")]
fn ensure_private(_path: &Path) -> Result<(), BridgeError> {
    Ok(())
}

/// Binds `127.0.0.1:<preferred>` when given and free, else `127.0.0.1:0`.
fn bind_loopback(preferred: Option<u16>) -> Result<TcpListener, BridgeError> {
    if let Some(port) = preferred.filter(|port| *port != 0) {
        match TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)) {
            Ok(listener) => return Ok(listener),
            Err(err) => runtime_log::log_info(format!("[browser_bridge] previous port {port} is not available ({err}); choosing a new one")),
        }
    }
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).map_err(|err| BridgeError::Bind { detail: err.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn first_start_writes_file_and_restart_reuses_secret_and_port() {
        let dir = TempDir::new("pairing-reuse");
        let (listener, first, owner) = bind_paired(dir.path()).unwrap();
        let file = read_bridge_file(&bridge_file_path(dir.path())).unwrap().unwrap();
        assert_eq!(file.protocol, PROTOCOL_VERSION);
        assert_eq!(file.port, first.port);
        assert_eq!(Secret::from_hex(&file.secret), Some(first.secret.clone()));
        drop(listener);
        drop(owner);

        let (_listener, second, _owner) = bind_paired(dir.path()).unwrap();
        assert_eq!(second.secret, first.secret);
        assert_eq!(second.port, first.port, "a free previous port is reused");
    }

    #[test]
    fn busy_previous_port_falls_back_and_rewrites_the_file() {
        let dir = TempDir::new("pairing-fallback");
        let (listener, first, owner) = bind_paired(dir.path()).unwrap();
        drop(owner);
        // Some other program now holds the previous port: the next start must move.
        let held = listener;
        let (_listener, second, _owner) = bind_paired(dir.path()).unwrap();
        assert_ne!(second.port, first.port);
        assert_eq!(second.secret, first.secret);
        let file = read_bridge_file(&bridge_file_path(dir.path())).unwrap().unwrap();
        assert_eq!(file.port, second.port);
        drop(held);
    }

    #[test]
    fn malformed_file_is_replaced_with_a_fresh_secret() {
        let dir = TempDir::new("pairing-malformed");
        fs::write(bridge_file_path(dir.path()), b"{not json").unwrap();
        let (_listener, pairing, _owner) = bind_paired(dir.path()).unwrap();
        let file = read_bridge_file(&bridge_file_path(dir.path())).unwrap().unwrap();
        assert_eq!(Secret::from_hex(&file.secret), Some(pairing.secret));
        // No temp file is left behind (the owner lock file stays by design).
        let mut leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().filter_map(Result::ok).map(|e| e.file_name()).collect();
        leftovers.sort();
        assert_eq!(leftovers, vec![std::ffi::OsString::from(OWNER_LOCK_FILE_NAME), std::ffi::OsString::from(CONFIG_FILE_NAME)]);
    }

    #[test]
    fn missing_folder_is_created() {
        let dir = TempDir::new("pairing-mkdir");
        let nested = dir.path().join("browser_extension").join("chromium");
        let (_listener, _pairing, _owner) = bind_paired(&nested).unwrap();
        assert!(bridge_file_path(&nested).is_file());
    }

    #[test]
    fn a_second_owner_of_the_folder_is_refused_and_touches_nothing() {
        let dir = TempDir::new("pairing-owner");
        let (_listener, first, owner) = bind_paired(dir.path()).unwrap();
        let before = fs::read(bridge_file_path(dir.path())).unwrap();
        // An independent open + lock, exactly what another app process does.
        assert_eq!(bind_paired(dir.path()).map(|(_, pairing, _)| pairing.port), Err(BridgeError::OwnedByOtherProcess));
        assert_eq!(fs::read(bridge_file_path(dir.path())).unwrap(), before, "the owner's pairing stays published");
        drop(owner);
        let (_listener2, second, _owner2) = bind_paired(dir.path()).unwrap();
        assert_eq!(second.secret, first.secret, "the next owner takes over the same pairing");
    }

    #[test]
    fn trust_rule_needs_a_regular_file_of_the_current_user() {
        assert_eq!(trust_verdict(true, true), None);
        assert_eq!(trust_verdict(true, false), Some("owned by another user"));
        assert_eq!(trust_verdict(false, true), Some("not a regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn a_valid_world_readable_file_keeps_its_secret_and_becomes_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("pairing-mode");
        let (listener, first, owner) = bind_paired(dir.path()).unwrap();
        let port = first.port;
        drop(listener);
        drop(owner);
        let path = bridge_file_path(dir.path());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let (_listener, second, _owner) = bind_paired(dir.path()).unwrap();
        assert_eq!(second.secret, first.secret, "the content is reused");
        assert_eq!(second.port, port, "nothing changed, so the file was not rewritten");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_leftover_temp_never_carries_the_secret_with_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("pairing-leftover-temp");
        let path = bridge_file_path(dir.path());
        // A crashed run's temp with a wide mode: the recipe reuses its name, so the 0600 mode
        // must be forced on the handle before the secret is written (no ensure_private here).
        let leftover = ms_atomic_file::temp_path_for(&path);
        fs::write(&leftover, b"stale").unwrap();
        fs::set_permissions(&leftover, fs::Permissions::from_mode(0o644)).unwrap();
        let file = BridgeFile { protocol: PROTOCOL_VERSION, port: 1, secret: "cd".repeat(32) };
        write_bridge_file(&path, &file).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(read_bridge_file(&path).unwrap(), Some(file));
        assert!(!leftover.exists(), "the temp was renamed into place");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_pairing_file_is_replaced_without_touching_its_target() {
        let dir = TempDir::new("pairing-symlink");
        let target = dir.path().join("elsewhere.json");
        let planted = BridgeFile { protocol: PROTOCOL_VERSION, port: 1, secret: "ab".repeat(32) };
        fs::write(&target, serde_json::to_vec(&planted).unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, bridge_file_path(dir.path())).unwrap();
        let (_listener, pairing, _owner) = bind_paired(dir.path()).unwrap();
        assert_ne!(pairing.secret.to_hex(), planted.secret, "a symlinked file's secret is not trusted");
        assert!(fs::symlink_metadata(bridge_file_path(dir.path())).unwrap().file_type().is_file());
        assert_eq!(serde_json::from_slice::<BridgeFile>(&fs::read(&target).unwrap()).unwrap(), planted);
    }
}

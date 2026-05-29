//! Server-side key recovery (SSSS) helpers.
//!
//! On a cold start after a local crypto-store wipe the device loses its
//! cross-signing private keys. With recovery enabled those keys (plus the
//! megolm backup decryption key) live in server-side Secure Secret Storage
//! (SSSS), encrypted under a recovery key. We persist that recovery key to
//! `{store_dir}/recovery.key` (mode 0600) so a fresh process can restore the
//! whole crypto identity without a human re-verifying the device.
//!
//! Every step here is best-effort: a recovery hiccup must NEVER crash the
//! daemon, so all failures are logged at `warn` and swallowed.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use matrix_sdk::Client;

const KEY_FILE: &str = "recovery.key";

/// Cold-start restore. If a persisted recovery key exists AND cross-signing is
/// NOT currently complete, use the key to restore cross-signing + secrets from
/// server-side SSSS. No-op (with a log) otherwise. Always best-effort.
///
/// Call this BEFORE the cross-signing bootstrap decision so restored keys make
/// the status complete and bootstrap is skipped.
pub(crate) async fn restore_if_needed(client: &Client, store_dir: &Path) {
    let key_path = store_dir.join(KEY_FILE);
    if !key_path.exists() {
        // Nothing to restore from; enable_and_persist_if_absent will create one
        // after cross-signing is bootstrapped.
        return;
    }

    let complete = match client.encryption().cross_signing_status().await {
        Some(status) => status.is_complete(),
        None => false,
    };
    if complete {
        // Keys already present locally; no restore needed.
        return;
    }

    let key = match std::fs::read_to_string(&key_path) {
        Ok(k) => k.trim().to_string(),
        Err(e) => {
            tracing::warn!(
                "recovery key file {} exists but could not be read: {e:#}",
                key_path.display()
            );
            return;
        }
    };

    tracing::info!(
        "cold start: cross-signing incomplete and recovery key present; \
         restoring secrets from server-side SSSS"
    );
    match client.encryption().recovery().recover(&key).await {
        Ok(()) => {
            tracing::info!("recovery restore succeeded; cross-signing secrets recovered from SSSS");
        }
        Err(e) => {
            tracing::warn!("recovery restore failed: {e:#}");
        }
    }
}

/// If no recovery key has been persisted yet, enable recovery (creating SSSS +
/// the server-side megolm backup) and persist the generated recovery key to
/// `{store_dir}/recovery.key` with mode 0600. Always best-effort.
///
/// Call this AFTER cross-signing keys exist so the secrets being stashed into
/// SSSS are present.
pub(crate) async fn enable_and_persist_if_absent(client: &Client, store_dir: &Path) {
    let key_path = store_dir.join(KEY_FILE);
    if key_path.exists() {
        // Already have a recovery key on disk; nothing to do. Log current state
        // for diagnostics.
        tracing::info!(
            "recovery key already present at {}; recovery state = {:?}",
            key_path.display(),
            client.encryption().recovery().state()
        );
        return;
    }

    // Deliberate: we key off the presence of the on-disk recovery key, not the
    // server-side recovery state. If the key file was lost but SSSS is still
    // enabled server-side, calling `enable()` rotates the secret-storage key and
    // we persist the fresh one. The old SSSS is orphaned but harmless, and this
    // is the correct trade-off: it restores our store-loss guarantee (a usable,
    // persisted recovery key), which is the whole point of this capability.
    tracing::info!("no recovery key on disk; enabling recovery (SSSS + key backup)");
    let recovery_key = match client.encryption().recovery().enable().await {
        Ok(key) => key,
        Err(e) => {
            tracing::warn!("failed to enable recovery: {e:#}");
            return;
        }
    };

    if let Err(e) = write_key_0600(&key_path, &recovery_key) {
        tracing::warn!(
            "recovery enabled but failed to persist key to {}: {e:#}",
            key_path.display()
        );
        return;
    }
    tracing::info!(
        "recovery enabled; recovery key persisted to {} (mode 0600)",
        key_path.display()
    );
}

/// Write `contents` to `path` with file mode 0600, creating or truncating.
fn write_key_0600(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    // Re-assert mode in case the file pre-existed with looser perms (create
    // does not change mode of an existing file).
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

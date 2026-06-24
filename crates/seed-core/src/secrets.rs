//! OS keystore access for share master seeds (Windows Credential Manager /
//! macOS Keychain / Linux Secret Service, via the `keyring` crate).
//!
//! Seeds are stored hex-encoded under service `seed-sync`, account = share id.
//! All operations are best-effort: on a headless box with no keystore (CI, some
//! servers) these return `Err`, and the engine falls back to storing the key in
//! its local DB. The seed is 32 bytes of ed25519 material.
//!
//! On Android the `keyring` crate has no backend, so it is compiled out entirely
//! (see the `cfg(target_os = "android")` stub below). `store_seed`/`load_seed`
//! deliberately return `Err` there so the engine takes its existing DB fallback
//! path (`seed_in_keyring = 0`), which already stores the full master key in
//! `state.db`. A future revision can back these with the Android Keystore via a
//! Kotlin callback; until then the DB fallback keeps master shares writable.

#[cfg(not(target_os = "android"))]
mod imp {
    const SERVICE: &str = "seed-sync";

    fn entry(share_id: &str) -> anyhow::Result<keyring::Entry> {
        Ok(keyring::Entry::new(SERVICE, share_id)?)
    }

    /// Store a master seed in the OS keystore.
    pub fn store_seed(share_id: &str, seed: &[u8; 32]) -> anyhow::Result<()> {
        let hex = data_encoding::HEXLOWER.encode(seed);
        entry(share_id)?.set_password(&hex)?;
        Ok(())
    }

    /// Load a master seed from the OS keystore, if present.
    pub fn load_seed(share_id: &str) -> anyhow::Result<[u8; 32]> {
        let hex = entry(share_id)?.get_password()?;
        let bytes = data_encoding::HEXLOWER.decode(hex.as_bytes())?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored seed has wrong length"))?;
        Ok(arr)
    }

    /// Remove a master seed from the OS keystore (best effort).
    pub fn delete_seed(share_id: &str) {
        if let Ok(e) = entry(share_id) {
            let _ = e.delete_credential();
        }
    }
}

#[cfg(target_os = "android")]
mod imp {
    //! Android has no `keyring` backend. These stubs force the engine onto its
    //! DB fallback: `store_seed` reports the keystore is unavailable, so
    //! `persist_share` stores the full master key in `state.db`
    //! (`seed_in_keyring = 0`), and `load_seed` is never reached for those rows
    //! (only consulted when `seed_in_keyring = 1`, which Android never sets).

    pub fn store_seed(_share_id: &str, _seed: &[u8; 32]) -> anyhow::Result<()> {
        anyhow::bail!("no OS keystore on Android; storing seed in DB")
    }

    pub fn load_seed(_share_id: &str) -> anyhow::Result<[u8; 32]> {
        anyhow::bail!("no OS keystore on Android")
    }

    pub fn delete_seed(_share_id: &str) {}
}

pub use imp::{delete_seed, load_seed, store_seed};

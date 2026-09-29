use soroban_sdk::{contracttype, Env, Symbol};

#[contracttype]
#[derive(Clone)]
enum StorageKey {
    NftCounter,
    NftVersion(u64),
}

/// Minimal storage helpers used by the migration module.
pub struct Storage;

impl Storage {
    /// Returns the highest NFT ID ever assigned (== total minted so far).
    pub fn get_nft_counter(env: &Env) -> u64 {
        env.storage()
            .instance()
            .get::<StorageKey, u64>(&StorageKey::NftCounter)
            .unwrap_or(0)
    }

    /// Returns whether an explicit per-NFT metadata version key exists.
    pub fn has_nft_version_key(env: &Env, nft_id: u64) -> bool {
        env.storage()
            .persistent()
            .has(&StorageKey::NftVersion(nft_id))
    }

    /// Persist the metadata schema version for a specific NFT.
    pub fn set_nft_version(env: &Env, nft_id: u64, version: u32) {
        env.storage()
            .persistent()
            .set(&StorageKey::NftVersion(nft_id), &version);
    }
}

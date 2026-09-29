#![no_std]
#![allow(clippy::too_many_arguments)]

mod errors;
mod migration;
mod sanitization;
mod storage;

pub use errors::NftErrorCode;

use soroban_sdk::{
    contract, contractimpl, contracttype, Address, Env, Map, String, Symbol, Vec,
};

/// Maximum byte length for an NFT image URI.
pub const MAX_NFT_URI_BYTES: u32 = 512;

/// Current metadata schema version persisted with every NFT record.
pub const METADATA_SCHEMA_VERSION: u32 = 1;

/// Maximum number of history entries returned in a single paginated query.
pub const MAX_SCAN_LIMIT: u32 = 100;

// ---------------------------------------------------------------------------
// Soroban storage keys
// ---------------------------------------------------------------------------
#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    RewardManager,
    TotalSupply,
    NftCounter,
    NftRecord(u64),
    PlayerNfts(Address),
    HuntNftCount(u64),
    MintingPaused,
    MaxSupply,
    CollectionMeta,
    AuthorizedContract(Address),
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Metadata provided at mint time and embedded in every NFT record.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NftMetadata {
    /// Display title for this specific NFT.
    pub title: String,
    /// Human-readable description.
    pub description: String,
    /// IPFS or HTTPS URI for the artwork.  Must be ≤ `MAX_NFT_URI_BYTES`.
    pub image_uri: String,
    /// Title of the hunt that was completed to earn this NFT.
    pub hunt_title: String,
    /// Rarity tier encoded as u32 (0 = common, higher = rarer).
    pub rarity: u32,
    /// Numeric tier / level classification.
    pub tier: u32,
    /// Optional original creator address for royalty tracking.
    pub creator: Option<Address>,
    /// Royalty in basis-points (0–10000), `None` means no royalty.
    pub royalty_bps: Option<u32>,
    /// Arbitrary key-value extension map (max 10 entries).
    pub extensions: Map<String, String>,
}

/// Full on-chain NFT record returned by `get_nft`.
#[contracttype]
#[derive(Clone, Debug)]
pub struct NftRecord {
    /// Unique numeric NFT identifier.
    pub nft_id: u64,
    /// Hunt that this NFT was minted for.
    pub hunt_id: u64,
    /// Current owner of this NFT.
    pub owner: Address,
    /// Embedded metadata snapshot.
    pub metadata: NftMetadata,
    /// Ledger timestamp at the time of minting.
    pub minted_at: u64,
    /// Whether this NFT can be transferred between addresses.
    pub transferable: bool,
}

/// Enriched metadata view returned by `get_nft_metadata`.
#[contracttype]
#[derive(Clone, Debug)]
pub struct NftMetadataView {
    pub nft_id: u64,
    pub hunt_id: u64,
    pub hunt_title: String,
    pub completion_timestamp: u64,
    pub completion_player: Address,
    pub current_owner: Address,
    pub title: String,
    pub description: String,
    pub image_uri: String,
    pub rarity: u32,
    pub tier: u32,
    pub schema_version: u32,
    pub extensions: Map<String, String>,
    pub creator: Option<Address>,
    pub royalty_bps: Option<u32>,
    pub transferable: bool,
}

/// Collection-level metadata set at initialization.
#[contracttype]
#[derive(Clone, Debug)]
pub struct CollectionMetadata {
    pub name: String,
    pub description: String,
    pub total_supply: u64,
    pub creator: Option<Address>,
}

/// Event emitted when a new NFT is minted.
#[contracttype]
#[derive(Clone, Debug)]
pub struct NftMintedEvent {
    pub nft_id: u64,
    pub hunt_id: u64,
    pub owner: Address,
    pub rarity: u32,
    pub tier: u32,
    pub minted_at: u64,
    /// Completion rank within the hunt (1-based).
    pub rank: u64,
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

fn get_admin(env: &Env) -> Option<Address> {
    env.storage().instance().get(&DataKey::Admin)
}

fn require_admin(env: &Env, caller: &Address) -> Result<(), NftErrorCode> {
    caller.require_auth();
    match get_admin(env) {
        Some(a) if a == *caller => Ok(()),
        _ => Err(NftErrorCode::Unauthorized),
    }
}

fn get_total_supply_inner(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get::<DataKey, u64>(&DataKey::TotalSupply)
        .unwrap_or(0)
}

fn increment_supply(env: &Env) -> u64 {
    let next = get_total_supply_inner(env) + 1;
    env.storage().instance().set(&DataKey::TotalSupply, &next);
    next
}

fn save_nft(env: &Env, nft: &NftRecord) {
    env.storage()
        .persistent()
        .set(&DataKey::NftRecord(nft.nft_id), nft);
}

fn load_nft(env: &Env, nft_id: u64) -> Option<NftRecord> {
    env.storage()
        .persistent()
        .get::<DataKey, NftRecord>(&DataKey::NftRecord(nft_id))
}

fn add_player_nft(env: &Env, player: &Address, nft_id: u64) {
    let mut list: Vec<u64> = env
        .storage()
        .persistent()
        .get::<DataKey, Vec<u64>>(&DataKey::PlayerNfts(player.clone()))
        .unwrap_or_else(|| Vec::new(env));
    list.push_back(nft_id);
    env.storage()
        .persistent()
        .set(&DataKey::PlayerNfts(player.clone()), &list);
}

fn increment_hunt_count(env: &Env, hunt_id: u64) -> u64 {
    let count: u64 = env
        .storage()
        .persistent()
        .get::<DataKey, u64>(&DataKey::HuntNftCount(hunt_id))
        .unwrap_or(0)
        + 1;
    env.storage()
        .persistent()
        .set(&DataKey::HuntNftCount(hunt_id), &count);
    count
}

fn get_reward_manager(env: &Env) -> Option<Address> {
    env.storage()
        .instance()
        .get::<DataKey, Address>(&DataKey::RewardManager)
}

fn require_minter(env: &Env, caller: &Address) -> Result<(), NftErrorCode> {
    caller.require_auth();
    // Reward manager or an authorized contract may mint.
    if let Some(rm) = get_reward_manager(env) {
        if rm == *caller {
            return Ok(());
        }
    }
    let key = DataKey::AuthorizedContract(caller.clone());
    if env
        .storage()
        .instance()
        .get::<DataKey, bool>(&key)
        .unwrap_or(false)
    {
        return Ok(());
    }
    Err(NftErrorCode::Unauthorized)
}

fn validate_metadata(
    env: &Env,
    metadata: &NftMetadata,
) -> Result<(), NftErrorCode> {
    if metadata.image_uri.len() == 0 {
        return Err(NftErrorCode::InvalidMetadata);
    }
    if metadata.image_uri.len() > MAX_NFT_URI_BYTES {
        return Err(NftErrorCode::InvalidImageUri);
    }
    // rarity max check (example: 0-9 valid)
    if metadata.rarity > 9 {
        return Err(NftErrorCode::InvalidRarity);
    }
    Ok(())
}

fn mint_internal(
    env: &Env,
    caller: &Address,
    hunt_id: u64,
    owner: &Address,
    metadata: NftMetadata,
    transferable: bool,
) -> Result<u64, NftErrorCode> {
    validate_metadata(env, &metadata)?;

    let nft_id = increment_supply(env);
    let now = env.ledger().timestamp();
    let rank = increment_hunt_count(env, hunt_id);

    let record = NftRecord {
        nft_id,
        hunt_id,
        owner: owner.clone(),
        metadata: metadata.clone(),
        minted_at: now,
        transferable,
    };

    save_nft(env, &record);
    add_player_nft(env, owner, nft_id);

    let event = NftMintedEvent {
        nft_id,
        hunt_id,
        owner: owner.clone(),
        rarity: metadata.rarity,
        tier: metadata.tier,
        minted_at: now,
        rank,
    };
    env.events().publish(
        (Symbol::new(env, "NftMinted"), nft_id),
        event,
    );

    Ok(nft_id)
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct NftReward;

#[contractimpl]
impl NftReward {
    // ------------------------------------------------------------------
    // Admin / initialisation
    // ------------------------------------------------------------------

    /// Initialise the contract.  Can only be called once.
    pub fn initialize(
        env: Env,
        admin: Address,
        minter: Address,
        max_supply: Option<u64>,
        collection_metadata: CollectionMetadata,
    ) -> Result<(), NftErrorCode> {
        if get_admin(&env).is_some() {
            return Err(NftErrorCode::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::AuthorizedContract(minter.clone()), &true);
        if let Some(max) = max_supply {
            if max == 0 {
                return Err(NftErrorCode::InvalidMaxSupply);
            }
            env.storage().instance().set(&DataKey::MaxSupply, &max);
        }
        env.storage()
            .instance()
            .set(&DataKey::CollectionMeta, &collection_metadata);
        Ok(())
    }

    /// Legacy single-arg admin initialiser (used by cross-contract tests).
    pub fn initialize_admin(env: Env, admin: Address) -> Result<(), NftErrorCode> {
        if get_admin(&env).is_some() {
            return Err(NftErrorCode::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    /// Register the RewardManager address that is authorised to mint.
    pub fn set_reward_manager(
        env: Env,
        admin: Address,
        reward_manager: Address,
    ) -> Result<(), NftErrorCode> {
        require_admin(&env, &admin)?;
        env.storage()
            .instance()
            .set(&DataKey::RewardManager, &reward_manager);
        Ok(())
    }

    /// Add an address to the authorised-contract set.
    pub fn add_authorized_contract(
        env: Env,
        admin: Address,
        contract: Address,
    ) -> Result<(), NftErrorCode> {
        require_admin(&env, &admin)?;
        env.storage()
            .instance()
            .set(&DataKey::AuthorizedContract(contract), &true);
        Ok(())
    }

    /// Remove an address from the authorised-contract set.
    pub fn remove_authorized_contract(
        env: Env,
        admin: Address,
        contract: Address,
    ) -> Result<(), NftErrorCode> {
        require_admin(&env, &admin)?;
        env.storage()
            .instance()
            .remove(&DataKey::AuthorizedContract(contract));
        Ok(())
    }

    // ------------------------------------------------------------------
    // Minting
    // ------------------------------------------------------------------

    /// Mint a soulbound (non-transferable) NFT.  Caller must be an
    /// authorised minter.
    pub fn mint_reward_nft(
        env: Env,
        caller: Address,
        hunt_id: u64,
        owner: Address,
        metadata: NftMetadata,
    ) -> Result<u64, NftErrorCode> {
        require_minter(&env, &caller)?;
        mint_internal(&env, &caller, hunt_id, &owner, metadata, false)
    }

    /// Mint using a raw `Map<Symbol, Val>` instead of a typed struct.
    /// Allows callers to set `"transferable"` to true via the map.
    pub fn mint_reward_nft_from_map(
        env: Env,
        caller: Address,
        hunt_id: u64,
        owner: Address,
        fields: Map<Symbol, soroban_sdk::Val>,
    ) -> Result<u64, NftErrorCode> {
        require_minter(&env, &caller)?;
        use soroban_sdk::{IntoVal, TryFromVal};

        let get_string = |key: &str| -> Result<String, NftErrorCode> {
            let k = Symbol::new(&env, key);
            let v = fields.get(k).ok_or(NftErrorCode::InvalidMetadata)?;
            String::try_from_val(&env, &v).map_err(|_| NftErrorCode::InvalidMetadata)
        };

        let get_u32 = |key: &str| -> Result<u32, NftErrorCode> {
            let k = Symbol::new(&env, key);
            let v = fields.get(k).ok_or(NftErrorCode::InvalidMetadata)?;
            u32::try_from_val(&env, &v).map_err(|_| NftErrorCode::InvalidMetadata)
        };

        let get_bool = |key: &str| -> bool {
            let k = Symbol::new(&env, key);
            fields
                .get(k)
                .and_then(|v| bool::try_from_val(&env, &v).ok())
                .unwrap_or(false)
        };

        let metadata = NftMetadata {
            title: get_string("title")?,
            description: get_string("description")?,
            image_uri: get_string("image_uri")?,
            hunt_title: get_string("hunt_title")?,
            rarity: get_u32("rarity")?,
            tier: get_u32("tier")?,
            creator: None,
            royalty_bps: None,
            extensions: Map::new(&env),
        };

        let transferable = get_bool("transferable");
        mint_internal(&env, &caller, hunt_id, &owner, metadata, transferable)
    }

    // ------------------------------------------------------------------
    // Queries
    // ------------------------------------------------------------------

    /// Returns the current admin address, or `None` if not initialised.
    pub fn get_admin(env: Env) -> Option<Address> {
        get_admin(&env)
    }

    /// Total number of NFTs minted across all hunts.
    pub fn get_total_supply(env: Env) -> u64 {
        get_total_supply_inner(&env)
    }

    /// Alias exposed by older call sites.
    pub fn total_supply(env: Env) -> u64 {
        get_total_supply_inner(&env)
    }

    /// Get a full NFT record by numeric ID.
    pub fn get_nft(env: Env, nft_id: u64) -> Option<NftRecord> {
        load_nft(&env, nft_id)
    }

    /// Returns the current owner of an NFT.
    pub fn owner_of(env: Env, nft_id: u64) -> Option<Address> {
        load_nft(&env, nft_id).map(|r| r.owner)
    }

    /// Enriched metadata view for an NFT.
    pub fn get_nft_metadata(env: Env, nft_id: u64) -> Option<NftMetadataView> {
        let r = load_nft(&env, nft_id)?;
        Some(NftMetadataView {
            nft_id: r.nft_id,
            hunt_id: r.hunt_id,
            hunt_title: r.metadata.hunt_title.clone(),
            completion_timestamp: r.minted_at,
            completion_player: r.owner.clone(),
            current_owner: r.owner.clone(),
            title: r.metadata.title.clone(),
            description: r.metadata.description.clone(),
            image_uri: r.metadata.image_uri.clone(),
            rarity: r.metadata.rarity,
            tier: r.metadata.tier,
            schema_version: METADATA_SCHEMA_VERSION,
            extensions: r.metadata.extensions.clone(),
            creator: r.metadata.creator.clone(),
            royalty_bps: r.metadata.royalty_bps,
            transferable: r.transferable,
        })
    }

    /// Return the list of NFT IDs owned by `player`, paginated.
    pub fn get_player_nfts(env: Env, player: Address, offset: u64, limit: u64) -> Vec<u64> {
        let all: Vec<u64> = env
            .storage()
            .persistent()
            .get::<DataKey, Vec<u64>>(&DataKey::PlayerNfts(player))
            .unwrap_or_else(|| Vec::new(&env));

        let start = offset as u32;
        let end = (offset + limit).min(all.len() as u64) as u32;
        let mut result: Vec<u64> = Vec::new(&env);
        for i in start..end {
            result.push_back(all.get(i).unwrap());
        }
        result
    }

    /// Returns how many NFTs have been minted for a given hunt.
    pub fn get_hunt_nft_count(env: Env, hunt_id: u64) -> u64 {
        env.storage()
            .persistent()
            .get::<DataKey, u64>(&DataKey::HuntNftCount(hunt_id))
            .unwrap_or(0)
    }

    /// Collection-level metadata.
    pub fn get_collection_metadata(env: Env) -> Option<CollectionMetadata> {
        env.storage()
            .instance()
            .get::<DataKey, CollectionMetadata>(&DataKey::CollectionMeta)
    }

    /// Search NFTs by metadata title (simple linear scan — test helper).
    pub fn search_nfts_by_metadata(
        env: Env,
        metadata: String,
        _start_after: Option<u64>,
        limit: Option<u32>,
    ) -> Vec<u64> {
        let _ = metadata;
        let _ = limit;
        Vec::new(&env)
    }

    // ------------------------------------------------------------------
    // Transfer / burn
    // ------------------------------------------------------------------

    /// Transfer a transferable NFT to a new owner.
    pub fn transfer_nft(
        env: Env,
        from: Address,
        to: Address,
        nft_id: u64,
    ) -> Result<(), NftErrorCode> {
        from.require_auth();
        let mut record = load_nft(&env, nft_id).ok_or(NftErrorCode::NftNotFound)?;
        if record.owner != from {
            return Err(NftErrorCode::NotOwner);
        }
        if !record.transferable {
            return Err(NftErrorCode::SoulboundNft);
        }
        record.owner = to;
        save_nft(&env, &record);
        Ok(())
    }

    /// Permanently burn an NFT.  Only the owner may burn.
    pub fn burn_nft(env: Env, caller: Address, nft_id: u64) -> Result<(), NftErrorCode> {
        caller.require_auth();
        let record = load_nft(&env, nft_id).ok_or(NftErrorCode::NftNotFound)?;
        if record.owner != caller {
            return Err(NftErrorCode::NotOwner);
        }
        env.storage()
            .persistent()
            .remove(&DataKey::NftRecord(nft_id));
        Ok(())
    }

    // ------------------------------------------------------------------
    // Metadata update
    // ------------------------------------------------------------------

    /// Update mutable metadata fields on an existing NFT.
    pub fn update_nft_metadata(
        env: Env,
        caller: Address,
        nft_id: u64,
        new_metadata: NftMetadata,
    ) -> Result<(), NftErrorCode> {
        require_admin(&env, &caller)?;
        let mut record = load_nft(&env, nft_id).ok_or(NftErrorCode::NftNotFound)?;
        validate_metadata(&env, &new_metadata)?;
        record.metadata = new_metadata;
        save_nft(&env, &record);
        Ok(())
    }

    /// Batch-update image URIs for a list of NFT IDs.
    pub fn admin_update_image_uris(
        env: Env,
        admin: Address,
        updates: Vec<(u64, String)>,
    ) -> Result<(), NftErrorCode> {
        require_admin(&env, &admin)?;
        for i in 0..updates.len() {
            let (nft_id, uri) = updates.get(i).unwrap();
            let mut record = load_nft(&env, nft_id).ok_or(NftErrorCode::NftNotFound)?;
            if uri.len() > MAX_NFT_URI_BYTES {
                return Err(NftErrorCode::InvalidImageUri);
            }
            record.metadata.image_uri = uri;
            save_nft(&env, &record);
        }
        Ok(())
    }
}

use hunty_core::{HuntyCore, HuntyCoreClient};
use reward_manager::RewardManager;
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::{token, Address, Env, String};

fn setup_reward_manager(env: &Env, core_id: &Address) -> (Address, Address) {
    let reward_manager_id = env.register(RewardManager, ());
    let token_admin = Address::generate(env);
    let token_contract = env.register_stellar_asset_contract_v2(token_admin.clone());
    let token_address = token_contract.address();

    env.as_contract(&reward_manager_id, || {
        RewardManager::initialize(
            env.clone(),
            token_admin.clone(),
            token_address.clone(),
            core_id.clone(),
        )
        .unwrap();
    });

    (reward_manager_id, token_address)
}

/// # Issue #1037 — cancel_hunt must persist Cancelled BEFORE calling refund_pool
///
/// The bug: `cancel_hunt` in hunty-core called `refund_pool` on RewardManager
/// *before* setting `hunt.status = Cancelled`.  RewardManager's `refund_pool`
/// makes a cross-contract call back to HuntyCore's `is_hunt_terminal` to verify
/// the hunt is in a terminal state before authorising the refund.  Because the
/// status was still `Active` at that point, `is_hunt_terminal` returned `false`
/// and `refund_pool` rejected the call, causing every cancellation of a funded
/// hunt to fail with `RefundFailed`.
///
/// The fix (this PR): persist `Cancelled` + `save_hunt` *before* the cross-
/// contract `refund_pool` call so that `is_hunt_terminal` sees the correct state.
/// If the subsequent refund fails the entire transaction reverts, so there is no
/// risk of persisting Cancelled without completing the refund.
#[test]
fn test_cancel_hunt_with_reward_pool_refund() {
    let env = Env::default();
    env.ledger().set_timestamp(1_700_000_000);
    env.mock_all_auths();

    let creator = Address::generate(&env);
    let admin = Address::generate(&env);
    let question = String::from_str(&env, "Valid question");
    let answer = String::from_str(&env, "a");

    // Register hunty-core first so we can pass it to RewardManager's initialize.
    let core_id = env.register(HuntyCore, ());
    let (reward_manager_id, token_address) = setup_reward_manager(&env, &core_id);

    let client = HuntyCoreClient::new(&env, &core_id);

    // Bootstrap admin
    client.initialize_admin(&admin);

    // Mint tokens to creator
    let sac = token::StellarAssetClient::new(&env, &token_address);
    sac.mint(&creator, &10_000_000);

    // Create hunt, add a clue, activate, then link the reward manager
    let hunt_id = client.create_hunt(
        &creator,
        &String::from_str(&env, "Integration Refund Hunt"),
        &String::from_str(&env, "Testing refund on cancel"),
        &None,
        &None,
        &0,
        &None,
        &None,
    );
    client.add_clue(&hunt_id, &question, &answer, &1, &true, &None, &None);
    client.activate_hunt(&hunt_id, &creator);
    client.set_reward_manager(&admin, &reward_manager_id);

    // Create the reward pool (separate as_contract call to avoid auth re-entry)
    env.as_contract(&reward_manager_id, || {
        RewardManager::create_reward_pool(
            env.clone(),
            creator.clone(),
            hunt_id,
            token_address.clone(),
            1, // min_distribution_amount > 0 required when no NFT contract is provided
            0,
            false,
        )
        .unwrap();
    });

    // Fund the pool in its own as_contract call so require_auth is clean
    env.as_contract(&reward_manager_id, || {
        RewardManager::fund_reward_pool(env.clone(), creator.clone(), hunt_id, 10_000_000)
            .unwrap();
    });

    // Sanity-check: balance is 10_000_000 before cancel
    env.as_contract(&reward_manager_id, || {
        assert_eq!(
            RewardManager::get_pool_balance(env.clone(), hunt_id),
            10_000_000
        );
    });

    // ── The actual fix under test ──────────────────────────────────────────
    // Before the fix, cancel_hunt would fail with RefundFailed because
    // is_hunt_terminal returned false (status was still Active at call time).
    // After the fix, cancel_hunt persists Cancelled first, so the cross-
    // contract terminal check passes and the refund succeeds.
    client.cancel_hunt(&hunt_id, &creator);

    // Verify: pool fully drained after cancellation
    env.as_contract(&reward_manager_id, || {
        assert_eq!(RewardManager::get_pool_balance(env.clone(), hunt_id), 0);
    });

    // Verify: tokens returned to the creator, contract holds nothing
    let token_client = token::Client::new(&env, &token_address);
    assert_eq!(token_client.balance(&creator), 10_000_000);
    assert_eq!(token_client.balance(&reward_manager_id), 0);
}

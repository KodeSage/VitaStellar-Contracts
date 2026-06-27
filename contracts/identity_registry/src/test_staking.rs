//! Unit tests for provider staking with real SUT token transfers.
//!
//! These tests exercise the phantom-stake fix: every `deposit_stake`,
//! `withdraw_stake` and `slash_stake` call must move real tokens via a
//! Stellar Asset Contract (SAC), so reputation can never be backed by stake
//! that was never locked.

extern crate std;

use super::*;
use crate::tests::{MockRbac, MockRbacClient};
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::{StellarAssetClient, TokenClient};
use soroban_sdk::{Address, Env, String};

const LOCK_SECONDS: u64 = 90 * 86_400;
const START_TS: u64 = 10_000;

struct StakingFixture {
    env: Env,
    client: IdentityRegistryContractClient<'static>,
    contract_id: Address,
    owner: Address,
    provider: Address,
    token_address: Address,
    token: TokenClient<'static>,
}

/// Deploys the RBAC mock, the identity registry and a real SAC token, mints
/// `provider_balance` to a freshly generated provider, and initialises the
/// registry with an admin owner.
fn setup(provider_balance: i128) -> StakingFixture {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(START_TS);

    let rbac_id = env.register_contract(None, MockRbac);
    let rbac_client = MockRbacClient::new(&env, &rbac_id);
    let contract_id = env.register_contract(None, IdentityRegistryContract);
    let client = IdentityRegistryContractClient::new(&env, &contract_id);

    let owner = Address::generate(&env);
    let _ = rbac_client.assign_role(&owner, &RbacRole::Admin);
    let network = String::from_str(&env, "testnet");
    client.initialize(&owner, &network, &rbac_id);

    // Real Stellar Asset Contract acting as the SUT token.
    let token_admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(token_admin);
    let token_address = sac.address();
    let token = TokenClient::new(&env, &token_address);

    let provider = Address::generate(&env);
    if provider_balance > 0 {
        StellarAssetClient::new(&env, &token_address).mint(&provider, &provider_balance);
    }

    StakingFixture {
        env,
        client,
        contract_id,
        owner,
        provider,
        token_address,
        token,
    }
}

// ---------------------------------------------------------------------------
// Deposit
// ---------------------------------------------------------------------------

#[test]
fn deposit_locks_real_tokens() {
    let f = setup(1_000);

    f.client.deposit_stake(&f.provider, &600, &f.token_address);

    // Tokens actually moved from the provider into the contract.
    assert_eq!(f.token.balance(&f.provider), 400);
    assert_eq!(f.token.balance(&f.contract_id), 600);

    // Stake ledger reflects the locked amount.
    assert_eq!(f.client.staked_balance(&f.provider), 600);
    let stake = f.client.get_stake(&f.provider).unwrap();
    assert_eq!(stake.amount, 600);
    assert!(!stake.slashed);
    assert_eq!(stake.locked_until, START_TS + LOCK_SECONDS);
}

#[test]
fn deposit_topup_accumulates_and_extends_lock() {
    let f = setup(1_000);

    f.client.deposit_stake(&f.provider, &300, &f.token_address);
    // Advance time, then top up.
    f.env.ledger().set_timestamp(START_TS + 1_000);
    f.client.deposit_stake(&f.provider, &200, &f.token_address);

    assert_eq!(f.token.balance(&f.contract_id), 500);
    assert_eq!(f.client.staked_balance(&f.provider), 500);
    let stake = f.client.get_stake(&f.provider).unwrap();
    // Lock extended to the later deposit's window.
    assert_eq!(stake.locked_until, START_TS + 1_000 + LOCK_SECONDS);
}

#[test]
fn deposit_rejects_invalid_token_contract() {
    let f = setup(1_000);
    // A random address that does not implement the token interface.
    let bogus = Address::generate(&f.env);

    let err = f
        .client
        .try_deposit_stake(&f.provider, &100, &bogus)
        .err()
        .unwrap()
        .unwrap();
    assert_eq!(err, Error::InvalidTokenContract);

    // No phantom stake recorded.
    assert_eq!(f.client.staked_balance(&f.provider), 0);
}

#[test]
fn deposit_rejects_non_positive_amount() {
    let f = setup(1_000);
    let err = f
        .client
        .try_deposit_stake(&f.provider, &0, &f.token_address)
        .err()
        .unwrap()
        .unwrap();
    assert_eq!(err, Error::InvalidInput);
}

#[test]
#[should_panic]
fn deposit_without_funds_fails() {
    // Provider has no balance; the token transfer must trap.
    let f = setup(0);
    f.client.deposit_stake(&f.provider, &100, &f.token_address);
}

// ---------------------------------------------------------------------------
// Withdraw
// ---------------------------------------------------------------------------

#[test]
fn withdraw_returns_tokens_after_lock() {
    let f = setup(1_000);
    f.client.deposit_stake(&f.provider, &600, &f.token_address);

    // Fast-forward past the lock period.
    f.env.ledger().set_timestamp(START_TS + LOCK_SECONDS + 1);

    let returned = f.client.withdraw_stake(&f.provider);
    assert_eq!(returned, 600);

    // Full balance is back with the provider; contract holds nothing.
    assert_eq!(f.token.balance(&f.provider), 1_000);
    assert_eq!(f.token.balance(&f.contract_id), 0);
    assert_eq!(f.client.staked_balance(&f.provider), 0);
    assert!(f.client.get_stake(&f.provider).is_none());
}

#[test]
fn withdraw_before_lock_fails() {
    let f = setup(1_000);
    f.client.deposit_stake(&f.provider, &600, &f.token_address);

    let err = f
        .client
        .try_withdraw_stake(&f.provider)
        .err()
        .unwrap()
        .unwrap();
    assert_eq!(err, Error::StakeLocked);

    // Tokens remain locked in the contract.
    assert_eq!(f.token.balance(&f.contract_id), 600);
}

#[test]
fn withdraw_without_stake_fails() {
    let f = setup(1_000);
    let err = f
        .client
        .try_withdraw_stake(&f.provider)
        .err()
        .unwrap()
        .unwrap();
    assert_eq!(err, Error::StakeNotFound);
}

// ---------------------------------------------------------------------------
// Slash
// ---------------------------------------------------------------------------

#[test]
fn slash_transfers_to_treasury() {
    let f = setup(1_000);
    let treasury = Address::generate(&f.env);
    f.client.set_treasury(&f.owner, &treasury);

    f.client.deposit_stake(&f.provider, &600, &f.token_address);

    let reason = String::from_str(&f.env, "fraud");
    f.client.slash_stake(&f.owner, &f.provider, &600, &reason);

    // Slashed funds went to the treasury, out of the contract.
    assert_eq!(f.token.balance(&treasury), 600);
    assert_eq!(f.token.balance(&f.contract_id), 0);

    let stake = f.client.get_stake(&f.provider).unwrap();
    assert!(stake.slashed);
    assert_eq!(stake.amount, 0);
}

#[test]
fn partial_slash_leaves_remainder() {
    let f = setup(1_000);
    let treasury = Address::generate(&f.env);
    f.client.set_treasury(&f.owner, &treasury);
    f.client.deposit_stake(&f.provider, &600, &f.token_address);

    let reason = String::from_str(&f.env, "partial");
    f.client.slash_stake(&f.owner, &f.provider, &200, &reason);

    assert_eq!(f.token.balance(&treasury), 200);
    assert_eq!(f.token.balance(&f.contract_id), 400);
    assert_eq!(f.client.staked_balance(&f.provider), 400);
}

#[test]
fn slashed_stake_cannot_be_withdrawn() {
    let f = setup(1_000);
    let treasury = Address::generate(&f.env);
    f.client.set_treasury(&f.owner, &treasury);
    f.client.deposit_stake(&f.provider, &600, &f.token_address);

    let reason = String::from_str(&f.env, "fraud");
    f.client.slash_stake(&f.owner, &f.provider, &200, &reason);

    f.env.ledger().set_timestamp(START_TS + LOCK_SECONDS + 1);

    let err = f
        .client
        .try_withdraw_stake(&f.provider)
        .err()
        .unwrap()
        .unwrap();
    assert_eq!(err, Error::StakeAlreadySlashed);
}

#[test]
fn slash_requires_treasury() {
    let f = setup(1_000);
    f.client.deposit_stake(&f.provider, &600, &f.token_address);

    let reason = String::from_str(&f.env, "fraud");
    let err = f
        .client
        .try_slash_stake(&f.owner, &f.provider, &600, &reason)
        .err()
        .unwrap()
        .unwrap();
    assert_eq!(err, Error::TreasuryNotSet);
}

#[test]
fn slash_requires_admin() {
    let f = setup(1_000);
    let treasury = Address::generate(&f.env);
    f.client.set_treasury(&f.owner, &treasury);
    f.client.deposit_stake(&f.provider, &600, &f.token_address);

    // A non-admin caller (even self-authorised under mock_all_auths) is rejected.
    let attacker = Address::generate(&f.env);
    let reason = String::from_str(&f.env, "fraud");
    let err = f
        .client
        .try_slash_stake(&attacker, &f.provider, &600, &reason)
        .err()
        .unwrap()
        .unwrap();
    assert_eq!(err, Error::Unauthorized);
}

#[test]
fn slash_amount_exceeding_stake_fails() {
    let f = setup(1_000);
    let treasury = Address::generate(&f.env);
    f.client.set_treasury(&f.owner, &treasury);
    f.client.deposit_stake(&f.provider, &600, &f.token_address);

    let reason = String::from_str(&f.env, "fraud");
    let err = f
        .client
        .try_slash_stake(&f.owner, &f.provider, &601, &reason)
        .err()
        .unwrap()
        .unwrap();
    assert_eq!(err, Error::InsufficientStake);
}

// ---------------------------------------------------------------------------
// Treasury & ledger helpers
// ---------------------------------------------------------------------------

#[test]
fn set_treasury_requires_admin() {
    let f = setup(0);
    let attacker = Address::generate(&f.env);
    let treasury = Address::generate(&f.env);
    let err = f
        .client
        .try_set_treasury(&attacker, &treasury)
        .err()
        .unwrap()
        .unwrap();
    assert_eq!(err, Error::Unauthorized);
}

#[test]
fn staked_balance_is_zero_without_stake() {
    let f = setup(0);
    assert_eq!(f.client.staked_balance(&f.provider), 0);
}

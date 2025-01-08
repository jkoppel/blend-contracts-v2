use cast::{i128, u64};
use sep_41_token::TokenClient;
use soroban_fixed_point_math::FixedPoint;
use soroban_sdk::{panic_with_error, unwrap::UnwrapOptimized, Address, Env};

use crate::{
    backstop::{load_pool_backstop_data, require_pool_above_threshold},
    constants::{BACKSTOP_EPOCH, SCALAR_14, SCALAR_7},
    dependencies::EmitterClient,
    errors::BackstopError,
    storage::{self, BackstopEmissionData, RzEmissionData},
    PoolBalance,
};

use super::distributor::update_emission_data;

/// Add a pool to the reward zone. If the reward zone is full, attempt to swap it with the pool to remove.
pub fn add_to_reward_zone(e: &Env, to_add: Address, to_remove: Option<Address>) {
    let mut reward_zone = storage::get_reward_zone(e);
    let rz_emission_index = storage::get_rz_emission_index(e);
    let max_rz_len = if e.ledger().timestamp() < BACKSTOP_EPOCH {
        10
    } else {
        // Max reward zone length is 50
        (10 + (i128(e.ledger().timestamp() - BACKSTOP_EPOCH) >> 23)).min(50) // bit-shift 23 is ~97 day interval
    };

    // ensure an entity in the reward zone cannot be included twice
    if reward_zone.contains(to_add.clone()) {
        panic_with_error!(e, BackstopError::BadRequest);
    }

    // enusre to_add has met the minimum backstop deposit threshold
    // NOTE: "to_add" can only carry a pool balance if it is a deployed pool from the factory
    let pool_data = load_pool_backstop_data(e, &to_add);
    if !require_pool_above_threshold(&pool_data) {
        panic_with_error!(e, BackstopError::InvalidRewardZoneEntry);
    }

    if max_rz_len > i128(reward_zone.len()) {
        // there is room in the reward zone. Add "to_add".
        reward_zone.push_front(to_add.clone());
    } else {
        match to_remove {
            None => panic_with_error!(e, BackstopError::RewardZoneFull),
            Some(to_remove) => {
                // swap to_add for to_remove
                let to_remove_index = reward_zone.first_index_of(to_remove.clone());
                match to_remove_index {
                    Some(idx) => {
                        // verify distribute was run recently to prevent "to_remove" from losing excess emissions
                        // @dev: resource constraints prevent us from distributing on reward zone changes
                        let last_distribution = storage::get_last_distribution_time(e);
                        if last_distribution < e.ledger().timestamp() - 24 * 60 * 60 {
                            panic_with_error!(e, BackstopError::BadRequest);
                        }

                        // Verify "to_add" has a higher backstop deposit that "to_remove"
                        if pool_data.tokens <= storage::get_pool_balance(e, &to_remove).tokens {
                            panic_with_error!(e, BackstopError::InvalidRewardZoneEntry);
                        }

                        // update backstop emissions for the pool before removing it from the reward zone
                        // set emission index to i128::MAX to prevent further emissions
                        let to_remove_emis_data =
                            storage::get_rz_emis_data(e, &to_remove).unwrap_optimized();
                        set_rz_emissions(
                            e,
                            &to_remove,
                            i128::MAX,
                            to_remove_emis_data.accrued,
                            false,
                        );

                        reward_zone.set(idx, to_add.clone());
                    }
                    None => panic_with_error!(e, BackstopError::InvalidRewardZoneEntry),
                }
            }
        }
    }
    // Set the new pool's backstop emissions index to the current gulp index
    if let Some(to_add_emis_data) = storage::get_rz_emis_data(e, &to_add) {
        set_rz_emissions(
            e,
            &to_add,
            rz_emission_index,
            to_add_emis_data.accrued,
            false,
        );
    } else {
        set_rz_emissions(e, &to_add, rz_emission_index, 0, false);
    }
    storage::set_reward_zone(e, &reward_zone);
}

/// remove a pool to the reward zone if below the minimum backstop deposit threshold
pub fn remove_from_reward_zone(e: &Env, to_remove: Address) {
    let mut reward_zone = storage::get_reward_zone(e);

    // enusre to_add has met the minimum backstop deposit threshold
    // NOTE: "to_add" can only carry a pool balance if it is a deployed pool from the factory
    let pool_data = load_pool_backstop_data(e, &to_remove);
    if require_pool_above_threshold(&pool_data) {
        panic_with_error!(e, BackstopError::BadRequest);
    } else {
        let to_remove_index = reward_zone.first_index_of(to_remove.clone());
        match to_remove_index {
            Some(idx) => {
                // verify distribute was run recently to prevent "to_remove" from losing excess emissions
                // @dev: resource constraints prevent us from distributing on reward zone changes
                let last_distribution = storage::get_last_distribution_time(e);
                if last_distribution < e.ledger().timestamp() - 24 * 60 * 60 {
                    panic_with_error!(e, BackstopError::BadRequest);
                }

                // update backstop emissions for the pool before removing it from the reward zone
                // set emission index to i128::MAX to prevent further emissions
                let to_remove_emis_data =
                    storage::get_rz_emis_data(e, &to_remove).unwrap_optimized();
                set_rz_emissions(e, &to_remove, i128::MAX, to_remove_emis_data.accrued, false);

                reward_zone.remove(idx);
                storage::set_reward_zone(e, &reward_zone);
            }
            None => panic_with_error!(e, BackstopError::InvalidRewardZoneEntry),
        }
    }
}

pub fn distribute(e: &Env) -> i128 {
    let reward_zone = storage::get_reward_zone(e);
    let rz_len = reward_zone.len();
    // reward zone must have at least one pool for emissions to start
    if rz_len == 0 {
        panic_with_error!(e, BackstopError::BadRequest);
    }
    let emitter = storage::get_emitter(e);
    let emitter_last_distribution =
        EmitterClient::new(&e, &emitter).get_last_distro(&e.current_contract_address());
    let last_distribution = storage::get_last_distribution_time(e);

    // ensure enough time has passed between the last emitter distribution and gulp_emissions
    // to prevent excess rounding issues
    if emitter_last_distribution <= (last_distribution + 60 * 60) {
        panic_with_error!(e, BackstopError::BadRequest);
    }
    storage::set_last_distribution_time(e, &emitter_last_distribution);
    let prev_index = storage::get_rz_emission_index(e);
    let new_emissions = i128(emitter_last_distribution - last_distribution) * SCALAR_7; // emitter releases 1 token per second

    // fetch total tokens of BLND in the reward zone
    let mut total_non_queued_tokens: i128 = 0;
    for rz_pool_index in 0..rz_len {
        let rz_pool = reward_zone.get(rz_pool_index).unwrap_optimized();
        let pool_balance = storage::get_pool_balance(e, &rz_pool);
        total_non_queued_tokens += pool_balance.non_queued_tokens();
    }

    let additional_index = new_emissions
        .fixed_div_floor(total_non_queued_tokens, SCALAR_14)
        .unwrap_optimized();
    let new_index = prev_index + additional_index;
    storage::set_rz_emission_index(e, &new_index);

    return new_emissions;
}

/// Assign backstop and pool emissions to `pool` based on the reward zone and the backstop emissions index
/// Returns the amount of backstop and pool emissions assigned to the pool
#[allow(clippy::zero_prefixed_literal)]
pub fn gulp_emissions(e: &Env, pool: &Address) -> (i128, i128) {
    let pool_balance = storage::get_pool_balance(e, pool);

    let new_emissions = update_rz_emis_data(e, pool, true);
    if new_emissions > 0 {
        let new_backstop_emissions = new_emissions
            .fixed_mul_floor(0_7000000, SCALAR_7)
            .unwrap_optimized();
        let new_pool_emissions = new_emissions
            .fixed_mul_floor(0_3000000, SCALAR_7)
            .unwrap_optimized();

        // distribute pool emissions via allowance to pools
        let blnd_token_client = TokenClient::new(e, &storage::get_blnd_token(e));
        let current_allowance = blnd_token_client.allowance(&e.current_contract_address(), pool);
        let new_seq = e.ledger().sequence() + storage::LEDGER_BUMP_USER; // ~120 days
        blnd_token_client.approve(
            &e.current_contract_address(),
            pool,
            &(current_allowance + new_pool_emissions),
            &new_seq,
        );
        set_backstop_emission_eps(e, &pool, &pool_balance, new_backstop_emissions);
        return (new_backstop_emissions, new_pool_emissions);
    }
    return (0, 0);
}

pub fn update_rz_emis_data(e: &Env, pool: &Address, to_gulp: bool) -> i128 {
    if let Some(emission_data) = storage::get_rz_emis_data(e, pool) {
        let pool_balance = storage::get_pool_balance(e, pool);
        let gulp_index = storage::get_rz_emission_index(e);
        let mut accrued = emission_data.accrued;
        if emission_data.index < gulp_index || to_gulp {
            if pool_balance.non_queued_tokens() > 0 {
                let new_emissions = pool_balance
                    .non_queued_tokens()
                    .fixed_mul_floor(gulp_index - emission_data.index, SCALAR_14)
                    .unwrap_optimized();
                accrued += new_emissions;
                return set_rz_emissions(e, pool, gulp_index, accrued, to_gulp);
            } else {
                return set_rz_emissions(e, pool, gulp_index, accrued, to_gulp);
            }
        }
    }
    return 0;
}

fn set_rz_emissions(e: &Env, pool_id: &Address, index: i128, accrued: i128, to_gulp: bool) -> i128 {
    if to_gulp {
        storage::set_rz_emis_data(e, pool_id, &RzEmissionData { index, accrued: 0 });
        accrued
    } else {
        storage::set_rz_emis_data(e, pool_id, &RzEmissionData { index, accrued });
        0
    }
}

/// Set a new EPS for the backstop
pub fn set_backstop_emission_eps(
    e: &Env,
    pool_id: &Address,
    pool_balance: &PoolBalance,
    new_tokens: i128,
) {
    let mut tokens_left_to_emit = new_tokens;
    let expiration = e.ledger().timestamp() + 7 * 24 * 60 * 60;

    if let Some(mut emission_data) = update_emission_data(e, pool_id, &pool_balance) {
        // a previous data exists - update with old data before setting new EPS
        if emission_data.last_time != e.ledger().timestamp() {
            // force the emission data to be updated to the current timestamp
            emission_data.last_time = e.ledger().timestamp();
        }
        // determine the amount of tokens not emitted from the last config
        if emission_data.expiration > e.ledger().timestamp() {
            let time_since_last_emission = emission_data.expiration - e.ledger().timestamp();

            // Eps is scaled by 14 decimal places
            let tokens_since_last_emission = i128(emission_data.eps)
                .fixed_mul_floor(i128(time_since_last_emission), SCALAR_7)
                .unwrap_optimized();
            tokens_left_to_emit += tokens_since_last_emission;
        }
        // Scale eps by 14 decimal places to reduce rounding errors
        let eps = u64(tokens_left_to_emit * SCALAR_7 / (7 * 24 * 60 * 60)).unwrap_optimized();
        emission_data.eps = eps;
        emission_data.expiration = expiration;
        storage::set_backstop_emis_data(e, pool_id, &emission_data);
    } else {
        // first time the pool's backstop is receiving emissions - ensure data is written
        let eps = u64(tokens_left_to_emit * SCALAR_7 / (7 * 24 * 60 * 60)).unwrap_optimized();
        storage::set_backstop_emis_data(
            e,
            pool_id,
            &BackstopEmissionData {
                eps,
                expiration,
                index: 0,
                last_time: e.ledger().timestamp(),
            },
        );
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger, LedgerInfo},
        vec, Vec,
    };

    use crate::{
        backstop::PoolBalance,
        testutils::{create_backstop, create_blnd_token, create_emitter},
    };

    /********** gulp_emissions **********/

    #[test]
    fn test_gulp_emissions() {
        let e = Env::default();
        e.budget().reset_unlimited();

        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop = create_backstop(&e);
        let emitter_distro_time = BACKSTOP_EPOCH - 10;
        let blnd_token_client = create_blnd_token(&e, &backstop, &Address::generate(&e)).1;
        create_emitter(
            &e,
            &backstop,
            &Address::generate(&e),
            &Address::generate(&e),
            emitter_distro_time,
        );
        let pool_1 = Address::generate(&e);
        let pool_2 = Address::generate(&e);
        let pool_3 = Address::generate(&e);
        let reward_zone: Vec<Address> = vec![&e, pool_1.clone(), pool_2.clone(), pool_3.clone()];

        // setup pool 1 to have ongoing emissions
        let pool_1_emissions_data = BackstopEmissionData {
            expiration: BACKSTOP_EPOCH + 1000,
            eps: 0_10000000000000,
            index: 8877660000000,
            last_time: BACKSTOP_EPOCH - 12345,
        };

        // setup pool 2 to have expired emissions
        let pool_2_emissions_data = BackstopEmissionData {
            expiration: BACKSTOP_EPOCH - 12345,
            eps: 0_05000000000000,
            index: 4532340000000,
            last_time: BACKSTOP_EPOCH - 12345,
        };
        // setup pool 3 to have no emissions
        e.as_contract(&backstop, || {
            storage::set_last_distribution_time(&e, &(emitter_distro_time - 7 * 24 * 60 * 60));
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_backstop_emis_data(&e, &pool_1, &pool_1_emissions_data);
            storage::set_rz_emis_data(
                &e,
                &pool_1,
                &RzEmissionData {
                    index: 0,
                    accrued: 0,
                },
            );
            storage::set_rz_emis_data(
                &e,
                &pool_2,
                &RzEmissionData {
                    index: 0,
                    accrued: 0,
                },
            );
            storage::set_rz_emis_data(
                &e,
                &pool_3,
                &RzEmissionData {
                    index: 0,
                    accrued: 0,
                },
            );
            storage::set_backstop_emis_data(&e, &pool_2, &pool_2_emissions_data);
            storage::set_pool_balance(
                &e,
                &pool_1,
                &PoolBalance {
                    tokens: 300_000_0000000,
                    shares: 200_000_0000000,
                    q4w: 0,
                },
            );
            storage::set_pool_balance(
                &e,
                &pool_2,
                &PoolBalance {
                    tokens: 200_000_0000000,
                    shares: 150_000_0000000,
                    q4w: 0,
                },
            );
            storage::set_pool_balance(
                &e,
                &pool_3,
                &PoolBalance {
                    tokens: 500_000_0000000,
                    shares: 600_000_0000000,
                    q4w: 0,
                },
            );
            blnd_token_client.approve(&backstop, &pool_1, &100_123_0000000, &e.ledger().sequence());

            distribute(&e);
            gulp_emissions(&e, &pool_1);
            gulp_emissions(&e, &pool_2);
            gulp_emissions(&e, &pool_3);

            assert_eq!(storage::get_last_distribution_time(&e), emitter_distro_time);
            assert_eq!(
                storage::get_pool_balance(&e, &pool_1).tokens,
                300_000_0000000
            );
            assert_eq!(
                storage::get_pool_balance(&e, &pool_2).tokens,
                200_000_0000000
            );
            assert_eq!(
                storage::get_pool_balance(&e, &pool_3).tokens,
                500_000_0000000
            );
            assert_eq!(
                blnd_token_client.allowance(&backstop, &pool_1),
                154_555_0000000
            );
            assert_eq!(
                blnd_token_client.allowance(&backstop, &pool_2),
                36_288_0000000
            );
            assert_eq!(
                blnd_token_client.allowance(&backstop, &pool_3),
                90_720_0000000
            );

            // validate backstop emissions

            let new_pool_1_data = storage::get_backstop_emis_data(&e, &pool_1).unwrap_optimized();
            assert_eq!(new_pool_1_data.eps, 0_21016534391534);
            assert_eq!(
                new_pool_1_data.expiration,
                BACKSTOP_EPOCH + 7 * 24 * 60 * 60
            );
            assert_eq!(new_pool_1_data.index, 9494910000000);
            assert_eq!(new_pool_1_data.last_time, BACKSTOP_EPOCH);

            let new_pool_2_data = storage::get_backstop_emis_data(&e, &pool_2).unwrap_optimized();
            assert_eq!(new_pool_2_data.eps, 0_14000000000000);
            assert_eq!(
                new_pool_2_data.expiration,
                BACKSTOP_EPOCH + 7 * 24 * 60 * 60
            );
            assert_eq!(new_pool_2_data.index, 4532340000000);
            assert_eq!(new_pool_2_data.last_time, BACKSTOP_EPOCH);

            let new_pool_3_data = storage::get_backstop_emis_data(&e, &pool_3).unwrap_optimized();
            assert_eq!(new_pool_3_data.eps, 0_35000000000000);
            assert_eq!(
                new_pool_3_data.expiration,
                BACKSTOP_EPOCH + 7 * 24 * 60 * 60
            );
            assert_eq!(new_pool_3_data.index, 0);
            assert_eq!(new_pool_3_data.last_time, BACKSTOP_EPOCH);
        });
    }

    /********** distribute **********/

    #[test]
    fn test_distribute() {
        let e = Env::default();
        e.budget().reset_unlimited();

        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop = create_backstop(&e);
        let emitter_distro_time = BACKSTOP_EPOCH - 10;
        create_emitter(
            &e,
            &backstop,
            &Address::generate(&e),
            &Address::generate(&e),
            emitter_distro_time,
        );

        let pool_1 = Address::generate(&e);
        let pool_2 = Address::generate(&e);
        let pool_3 = Address::generate(&e);
        let reward_zone: Vec<Address> = vec![&e, pool_1.clone(), pool_2.clone(), pool_3.clone()];

        e.as_contract(&backstop, || {
            storage::set_last_distribution_time(&e, &(emitter_distro_time - (60 * 60 * 24)));
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_pool_balance(
                &e,
                &pool_1,
                &PoolBalance {
                    tokens: 300_000_0000000,
                    shares: 200_000_0000000,
                    q4w: 0,
                },
            );
            storage::set_pool_balance(
                &e,
                &pool_2,
                &PoolBalance {
                    tokens: 200_000_0000000,
                    shares: 150_000_0000000,
                    q4w: 0,
                },
            );
            storage::set_pool_balance(
                &e,
                &pool_3,
                &PoolBalance {
                    tokens: 500_000_0000000,
                    shares: 600_000_0000000,
                    q4w: 0,
                },
            );

            distribute(&e);

            let gulp_index = storage::get_rz_emission_index(&e);
            assert_eq!(gulp_index, 8640000000000);
        });
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1000)")]
    fn test_distribute_empty_rz() {
        let e = Env::default();
        e.budget().reset_unlimited();

        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop = create_backstop(&e);
        let emitter_distro_time = BACKSTOP_EPOCH - 10;
        create_emitter(
            &e,
            &backstop,
            &Address::generate(&e),
            &Address::generate(&e),
            emitter_distro_time,
        );

        let pool_1 = Address::generate(&e);

        let reward_zone: Vec<Address> = vec![&e];

        e.as_contract(&backstop, || {
            storage::set_last_distribution_time(&e, &(emitter_distro_time - (60 * 60 * 24)));
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_pool_balance(
                &e,
                &pool_1,
                &PoolBalance {
                    tokens: 300_000_0000000,
                    shares: 200_000_0000000,
                    q4w: 0,
                },
            );

            distribute(&e);
        });
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1000)")]
    fn test_distribute_too_soon() {
        let e = Env::default();
        e.budget().reset_unlimited();

        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop = create_backstop(&e);
        create_blnd_token(&e, &backstop, &Address::generate(&e)).1;
        let emitter_distro_time = BACKSTOP_EPOCH - 1000;
        create_emitter(
            &e,
            &backstop,
            &Address::generate(&e),
            &Address::generate(&e),
            emitter_distro_time,
        );
        let blnd_token_client = create_blnd_token(&e, &backstop, &Address::generate(&e)).1;

        let pool_1 = Address::generate(&e);
        let pool_2 = Address::generate(&e);
        let pool_3 = Address::generate(&e);
        let reward_zone: Vec<Address> = vec![&e, pool_1.clone(), pool_2.clone(), pool_3.clone()];

        // setup pool 1 to have ongoing emissions

        let pool_1_emissions_data = BackstopEmissionData {
            expiration: BACKSTOP_EPOCH + 1000,
            eps: 0_1000000,
            index: 887766,
            last_time: BACKSTOP_EPOCH - 12345,
        };

        // setup pool 2 to have expired emissions

        let pool_2_emissions_data = BackstopEmissionData {
            expiration: BACKSTOP_EPOCH - 12345,
            eps: 0_0500000,
            index: 453234,
            last_time: BACKSTOP_EPOCH - 12345,
        };
        // setup pool 3 to have no emissions
        e.as_contract(&backstop, || {
            storage::set_last_distribution_time(&e, &(emitter_distro_time - 59 * 60));
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_backstop_emis_data(&e, &pool_1, &pool_1_emissions_data);
            blnd_token_client.approve(&backstop, &pool_1, &100_123_0000000, &e.ledger().sequence());

            storage::set_backstop_emis_data(&e, &pool_2, &pool_2_emissions_data);
            storage::set_pool_balance(
                &e,
                &pool_1,
                &PoolBalance {
                    tokens: 300_000_0000000,
                    shares: 200_000_0000000,
                    q4w: 0,
                },
            );
            storage::set_pool_balance(
                &e,
                &pool_2,
                &PoolBalance {
                    tokens: 200_000_0000000,
                    shares: 150_000_0000000,
                    q4w: 0,
                },
            );
            storage::set_pool_balance(
                &e,
                &pool_3,
                &PoolBalance {
                    tokens: 500_000_0000000,
                    shares: 600_000_0000000,
                    q4w: 0,
                },
            );

            distribute(&e);
        });
    }

    #[test]
    fn test_distribute_gulp_no_overflow() {
        let e = Env::default();
        e.budget().reset_unlimited();

        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH + 10_000_000_000,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop = create_backstop(&e);
        let (blnd_address, _) = create_blnd_token(&e, &backstop, &Address::generate(&e));

        // Distribute 1 trillion tokens to 1 backstop token
        let emitter_distro_time = BACKSTOP_EPOCH + 10_000_000_000;
        create_emitter(
            &e,
            &backstop,
            &Address::generate(&e),
            &blnd_address,
            emitter_distro_time,
        );
        let pool_1 = Address::generate(&e);
        let reward_zone: Vec<Address> = vec![&e, pool_1.clone()];

        e.as_contract(&backstop, || {
            storage::set_last_distribution_time(&e, &(&BACKSTOP_EPOCH));
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_rz_emis_data(
                &e,
                &pool_1,
                &RzEmissionData {
                    index: 0,
                    accrued: 0,
                },
            );
            storage::set_pool_balance(
                &e,
                &pool_1,
                &PoolBalance {
                    tokens: 1_0000000,
                    shares: 1_0000000,
                    q4w: 0,
                },
            );

            let gulp_index = storage::get_rz_emission_index(&e);
            assert_eq!(gulp_index, 0);
        });
        // Distribute 1 trillion tokens to 1 backstop token
        for _ in 0..100 {
            e.as_contract(&backstop, || {
                distribute(&e);
                gulp_emissions(&e, &pool_1);
            });
            e.ledger().set(LedgerInfo {
                timestamp: e.ledger().timestamp() + 10_000_000_000,
                protocol_version: 22,
                sequence_number: 0,
                network_id: Default::default(),
                base_reserve: 10,
                min_temp_entry_ttl: 10,
                min_persistent_entry_ttl: 10,
                max_entry_ttl: 3110400,
            });
            create_emitter(
                &e,
                &backstop,
                &Address::generate(&e),
                &blnd_address,
                e.ledger().timestamp() + 10_000_000_000,
            );
        }
        e.as_contract(&backstop, || {
            let gulp_index = storage::get_rz_emission_index(&e);
            assert_eq!(gulp_index, 101000000000000000000000000);
        });
    }

    /********** add_to_reward_zone **********/

    #[test]
    fn test_add_to_rz_empty_adds_pool() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            base_reserve: 10,
            network_id: Default::default(),
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let to_add = Address::generate(&e);

        e.as_contract(&backstop_id, || {
            storage::set_pool_balance(
                &e,
                &to_add,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));

            add_to_reward_zone(&e, to_add.clone(), None);
            let actual_rz = storage::get_reward_zone(&e);
            let expected_rz: Vec<Address> = vec![&e, to_add];
            assert_eq!(actual_rz, expected_rz);
        });
    }

    #[test]
    fn test_add_to_rz_before_epoch_max_10() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH - 100000,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let to_add = Address::generate(&e);
        let mut reward_zone: Vec<Address> = vec![
            &e,
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
        ];

        e.as_contract(&backstop_id, || {
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_pool_balance(
                &e,
                &to_add,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));

            add_to_reward_zone(&e, to_add.clone(), None);
            let actual_rz = storage::get_reward_zone(&e);
            reward_zone.push_front(to_add);
            assert_eq!(actual_rz, reward_zone);
        });
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1002)")]
    fn test_add_to_rz_empty_pool_under_backstop_threshold() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            base_reserve: 10,
            network_id: Default::default(),
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let to_add = Address::generate(&e);

        e.as_contract(&backstop_id, || {
            storage::set_pool_balance(
                &e,
                &to_add,
                &PoolBalance {
                    shares: 100_000_0000000,
                    tokens: 75_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));

            add_to_reward_zone(&e, to_add.clone(), None);
            let actual_rz = storage::get_reward_zone(&e);
            let expected_rz: Vec<Address> = vec![&e, to_add];
            assert_eq!(actual_rz, expected_rz);
        });
    }

    #[test]
    fn test_add_to_rz_increases_size_over_time() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH + (1 << 23),
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let to_add = Address::generate(&e);
        let mut reward_zone: Vec<Address> = vec![
            &e,
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
        ];

        e.as_contract(&backstop_id, || {
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_pool_balance(
                &e,
                &to_add,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));

            add_to_reward_zone(&e, to_add.clone(), None);
            let actual_rz = storage::get_reward_zone(&e);
            reward_zone.push_front(to_add);
            assert_eq!(actual_rz, reward_zone);
        });
    }
    #[test]
    #[should_panic(expected = "Error(Contract, #1009)")]
    fn test_add_to_rz_respects_max_size() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            // Allow enough time for 100 pools to be added
            timestamp: BACKSTOP_EPOCH + (1 << 23) * 100,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let to_add = Address::generate(&e);
        let mut reward_zone: Vec<Address> = vec![&e];
        for _ in 0..50 {
            reward_zone.push_back(Address::generate(&e));
        }
        e.as_contract(&backstop_id, || {
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_pool_balance(
                &e,
                &to_add,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));

            // This should fail due to the reward zone being full and not having a pool to remove
            add_to_reward_zone(&e, to_add.clone(), None);
        });
    }
    #[test]
    #[should_panic(expected = "Error(Contract, #1009)")]
    fn test_add_to_rz_takes_floor_for_size() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH + (1 << 23) - 1,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let to_add = Address::generate(&e);
        let reward_zone: Vec<Address> = vec![
            &e,
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
        ];

        e.as_contract(&backstop_id, || {
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_pool_balance(
                &e,
                &to_add,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));

            add_to_reward_zone(&e, to_add.clone(), None);
        });
    }

    #[test]
    fn test_add_to_rz_swap_happy_path() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        create_blnd_token(&e, &backstop_id, &Address::generate(&e));

        let to_add = Address::generate(&e);
        let to_remove = Address::generate(&e);
        let mut reward_zone: Vec<Address> = vec![
            &e,
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            to_remove.clone(), // index 7
            Address::generate(&e),
            Address::generate(&e),
        ];

        e.as_contract(&backstop_id, || {
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_last_distribution_time(&e, &(BACKSTOP_EPOCH - 1 * 24 * 60 * 60));
            storage::set_pool_balance(
                &e,
                &to_add,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_001_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_pool_balance(
                &e,
                &to_remove,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_backstop_emis_data(
                &e,
                &to_remove,
                &BackstopEmissionData {
                    eps: 0_10000000000000,
                    expiration: BACKSTOP_EPOCH + 1000,
                    index: 0,
                    last_time: BACKSTOP_EPOCH - 12345,
                },
            );
            storage::set_rz_emis_data(
                &e,
                &to_remove,
                &RzEmissionData {
                    index: (1234 * SCALAR_7),
                    accrued: 0,
                },
            );
            storage::set_rz_emission_index(&e, &(5678 * SCALAR_7));
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));
            add_to_reward_zone(&e, to_add.clone(), Some(to_remove.clone()));
            let actual_rz = storage::get_reward_zone(&e);
            assert_eq!(actual_rz.len(), 10);
            reward_zone.set(7, to_add.clone());
            assert_eq!(actual_rz, reward_zone);

            let to_remove_emis_data = storage::get_rz_emis_data(&e, &to_remove).unwrap_optimized();
            let to_add_emis_data = storage::get_rz_emis_data(&e, &to_add).unwrap_optimized();
            assert_eq!(to_add_emis_data.index, 5678 * SCALAR_7);
            assert_eq!(to_remove_emis_data.index, i128::MAX);
        });
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1002)")]
    fn test_add_to_rz_swap_not_enough_tokens() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let to_add = Address::generate(&e);
        let to_remove = Address::generate(&e);
        let reward_zone: Vec<Address> = vec![
            &e,
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            to_remove.clone(), // index 7
            Address::generate(&e),
            Address::generate(&e),
        ];

        e.as_contract(&backstop_id, || {
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_last_distribution_time(&e, &(BACKSTOP_EPOCH - 1 * 24 * 60 * 60));
            storage::set_pool_balance(
                &e,
                &to_add,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_pool_balance(
                &e,
                &to_remove,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));

            add_to_reward_zone(&e, to_add.clone(), Some(to_remove));
        });
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1000)")]
    fn test_add_to_rz_swap_distribution_too_long_ago() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let to_add = Address::generate(&e);
        let to_remove = Address::generate(&e);
        let reward_zone: Vec<Address> = vec![
            &e,
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            to_remove.clone(), // index 7
            Address::generate(&e),
            Address::generate(&e),
        ];

        e.as_contract(&backstop_id, || {
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_last_distribution_time(&e, &(BACKSTOP_EPOCH - 1 * 24 * 60 * 60 - 1));
            storage::set_pool_balance(
                &e,
                &to_add,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_001_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_pool_balance(
                &e,
                &to_remove,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));

            add_to_reward_zone(&e, to_add.clone(), Some(to_remove));
        });
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1002)")]
    fn test_add_to_rz_to_remove_not_in_rz() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let to_add = Address::generate(&e);
        let to_remove = Address::generate(&e);
        let reward_zone: Vec<Address> = vec![
            &e,
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
        ];

        e.as_contract(&backstop_id, || {
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_last_distribution_time(&e, &(BACKSTOP_EPOCH - 24 * 60 * 60));
            storage::set_pool_balance(
                &e,
                &to_add,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_001_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_pool_balance(
                &e,
                &to_remove,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));

            add_to_reward_zone(&e, to_add.clone(), Some(to_remove));
        });
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1000)")]
    fn test_add_to_rz_already_exists_panics() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let to_add = Address::generate(&e);
        let to_remove = Address::generate(&e);
        let reward_zone: Vec<Address> = vec![
            &e,
            Address::generate(&e),
            to_remove.clone(),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            Address::generate(&e),
            to_add.clone(),
            Address::generate(&e),
            Address::generate(&e),
        ];

        e.as_contract(&backstop_id, || {
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_last_distribution_time(&e, &(BACKSTOP_EPOCH - 24 * 60 * 60));
            storage::set_pool_balance(
                &e,
                &to_add,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_001_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_pool_balance(
                &e,
                &to_remove,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));

            add_to_reward_zone(&e, to_add.clone(), Some(to_remove.clone()));
        });
    }

    /********** remove_from_reward_zone **********/

    #[test]
    fn test_remove_from_rz() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        create_blnd_token(&e, &backstop_id, &Address::generate(&e));

        let to_remove = Address::generate(&e);
        let mut reward_zone: Vec<Address> = vec![
            &e,
            Address::generate(&e),
            to_remove.clone(), // index 7
        ];

        e.as_contract(&backstop_id, || {
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_last_distribution_time(&e, &(BACKSTOP_EPOCH - 1 * 24 * 60 * 60));
            storage::set_pool_balance(
                &e,
                &to_remove,
                &PoolBalance {
                    shares: 90_000_0000000,
                    tokens: 100_001_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_pool_balance(
                &e,
                &to_remove,
                &PoolBalance {
                    shares: 35_000_0000000,
                    tokens: 40_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_backstop_emis_data(
                &e,
                &to_remove,
                &BackstopEmissionData {
                    eps: 0_10000000000000,
                    expiration: BACKSTOP_EPOCH + 1000,
                    index: 0,
                    last_time: BACKSTOP_EPOCH - 12345,
                },
            );
            storage::set_rz_emis_data(&e, &to_remove, {
                &RzEmissionData {
                    index: 1234 * SCALAR_7,
                    accrued: 0,
                }
            });
            storage::set_rz_emission_index(&e, &(5678 * SCALAR_7));
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));
            remove_from_reward_zone(&e, to_remove.clone());
            let actual_rz = storage::get_reward_zone(&e);
            reward_zone.remove(1);
            assert_eq!(actual_rz.len(), 1);
            assert_eq!(actual_rz, reward_zone);

            let to_remove_rz_emis_data =
                storage::get_rz_emis_data(&e, &to_remove).unwrap_optimized();
            assert_eq!(to_remove_rz_emis_data.index, i128::MAX);
        });
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1000)")]
    fn test_remove_from_rz_above_threshold() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        create_blnd_token(&e, &backstop_id, &Address::generate(&e));

        let to_remove = Address::generate(&e);
        let reward_zone: Vec<Address> = vec![
            &e,
            Address::generate(&e),
            to_remove.clone(), // index 7
        ];

        e.as_contract(&backstop_id, || {
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_last_distribution_time(&e, &(BACKSTOP_EPOCH - 1 * 24 * 60 * 60));
            storage::set_pool_balance(
                &e,
                &to_remove,
                &PoolBalance {
                    shares: 80_000_0000000,
                    tokens: 90_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_backstop_emis_data(
                &e,
                &to_remove,
                &BackstopEmissionData {
                    eps: 0_10000000000000,
                    expiration: BACKSTOP_EPOCH + 1000,
                    index: 0,
                    last_time: BACKSTOP_EPOCH - 12345,
                },
            );
            storage::set_rz_emis_data(&e, &to_remove, {
                &RzEmissionData {
                    index: 1234 * SCALAR_7,
                    accrued: 0,
                }
            });
            storage::set_rz_emission_index(&e, &(5678 * SCALAR_7));
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));
            remove_from_reward_zone(&e, to_remove.clone());
        });
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1000)")]
    fn test_remove_from_rz_last_distribution_too_long_ago() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        create_blnd_token(&e, &backstop_id, &Address::generate(&e));

        let to_remove = Address::generate(&e);
        let reward_zone: Vec<Address> = vec![
            &e,
            Address::generate(&e),
            to_remove.clone(), // index 7
        ];

        e.as_contract(&backstop_id, || {
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_last_distribution_time(&e, &(BACKSTOP_EPOCH - 1 * 24 * 60 * 60 - 1));
            storage::set_pool_balance(
                &e,
                &to_remove,
                &PoolBalance {
                    shares: 80_000_0000000,
                    tokens: 90_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_backstop_emis_data(
                &e,
                &to_remove,
                &BackstopEmissionData {
                    eps: 0_10000000000000,
                    expiration: BACKSTOP_EPOCH + 1000,
                    index: 0,
                    last_time: BACKSTOP_EPOCH - 12345,
                },
            );
            storage::set_rz_emis_data(&e, &to_remove, {
                &RzEmissionData {
                    index: 1234 * SCALAR_7,
                    accrued: 0,
                }
            });
            storage::set_rz_emission_index(&e, &(5678 * SCALAR_7));
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));
            remove_from_reward_zone(&e, to_remove.clone());
        });
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1002)")]
    fn test_remove_from_rz_not_in_rz() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        create_blnd_token(&e, &backstop_id, &Address::generate(&e));

        let to_remove = Address::generate(&e);
        let reward_zone: Vec<Address> = vec![&e, Address::generate(&e)];

        e.as_contract(&backstop_id, || {
            storage::set_reward_zone(&e, &reward_zone);
            storage::set_last_distribution_time(&e, &(BACKSTOP_EPOCH - 1 * 24 * 60 * 60));
            storage::set_pool_balance(
                &e,
                &to_remove,
                &PoolBalance {
                    shares: 35_000_0000000,
                    tokens: 40_000_0000000,
                    q4w: 1_000_0000000,
                },
            );
            storage::set_backstop_emis_data(
                &e,
                &to_remove,
                &BackstopEmissionData {
                    eps: 0_10000000000000,
                    expiration: BACKSTOP_EPOCH + 1000,
                    index: 0,
                    last_time: BACKSTOP_EPOCH - 12345,
                },
            );
            storage::set_rz_emis_data(&e, &to_remove, {
                &RzEmissionData {
                    index: 1234 * SCALAR_7,
                    accrued: 0,
                }
            });
            storage::set_rz_emission_index(&e, &(5678 * SCALAR_7));
            storage::set_lp_token_val(&e, &(5_0000000, 0_1000000));
            remove_from_reward_zone(&e, to_remove.clone());
        });
    }

    /********** update_rz_emis_data **********/

    #[test]
    fn test_update_rz_emis_data() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });
        let backstop_id = create_backstop(&e);
        let pool = Address::generate(&e);

        e.as_contract(&backstop_id, || {
            storage::set_rz_emission_index(&e, &22_00000000000000);
            storage::set_rz_emis_data(
                &e,
                &pool,
                &RzEmissionData {
                    index: 11_00000000000000,
                    accrued: 100_0000000,
                },
            );
            storage::set_pool_balance(
                &e,
                &pool,
                &PoolBalance {
                    shares: 150_0000000,
                    tokens: 200_0000000,
                    q4w: 2_0000000,
                },
            );
            let result = update_rz_emis_data(&e, &pool, false);
            let actual_data = storage::get_rz_emis_data(&e, &pool).unwrap_optimized();
            assert_eq!(result, 0);
            assert_eq!(actual_data.index, 22_00000000000000);
            assert_eq!(actual_data.accrued, 2270_6666674);
        });
    }

    #[test]
    fn test_update_rz_emis_data_consumes_accrued() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });
        let backstop_id = create_backstop(&e);
        let pool = Address::generate(&e);

        e.as_contract(&backstop_id, || {
            storage::set_rz_emission_index(&e, &22_00000000000000);
            storage::set_rz_emis_data(
                &e,
                &pool,
                &RzEmissionData {
                    index: 11_00000000000000,
                    accrued: 100_0000000,
                },
            );
            storage::set_pool_balance(
                &e,
                &pool,
                &PoolBalance {
                    shares: 150_0000000,
                    tokens: 200_0000000,
                    q4w: 2_0000000,
                },
            );
            let result = update_rz_emis_data(&e, &pool, true);
            let actual_data = storage::get_rz_emis_data(&e, &pool).unwrap_optimized();
            assert_eq!(result, 22706666674);
            assert_eq!(actual_data.index, 22_00000000000000);
            assert_eq!(actual_data.accrued, 0);
        });
    }

    #[test]
    fn test_update_rz_emis_data_zero_pool_tokens() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });
        let backstop_id = create_backstop(&e);
        let pool = Address::generate(&e);

        e.as_contract(&backstop_id, || {
            storage::set_rz_emission_index(&e, &22_00000000000000);
            storage::set_rz_emis_data(
                &e,
                &pool,
                &RzEmissionData {
                    index: 11_00000000000000,
                    accrued: 100_0000000,
                },
            );
            storage::set_pool_balance(
                &e,
                &pool,
                &PoolBalance {
                    shares: 150_0000000,
                    tokens: 0,
                    q4w: 2_0000000,
                },
            );
            let result = update_rz_emis_data(&e, &pool, false);
            let actual_data = storage::get_rz_emis_data(&e, &pool).unwrap_optimized();
            assert_eq!(result, 0);
            assert_eq!(actual_data.index, 22_00000000000000);
            assert_eq!(actual_data.accrued, 100_0000000);
        });
    }

    #[test]
    fn test_update_rz_emis_data_gulp_zero_pool_tokens() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });
        let backstop_id = create_backstop(&e);
        let pool = Address::generate(&e);

        e.as_contract(&backstop_id, || {
            storage::set_rz_emission_index(&e, &22_00000000000000);
            storage::set_rz_emis_data(
                &e,
                &pool,
                &RzEmissionData {
                    index: 11_00000000000000,
                    accrued: 100_0000000,
                },
            );
            storage::set_pool_balance(
                &e,
                &pool,
                &PoolBalance {
                    shares: 150_0000000,
                    tokens: 0,
                    q4w: 2_0000000,
                },
            );
            let result = update_rz_emis_data(&e, &pool, true);
            let actual_data = storage::get_rz_emis_data(&e, &pool).unwrap_optimized();
            assert_eq!(result, 100_0000000);
            assert_eq!(actual_data.index, 22_00000000000000);
            assert_eq!(actual_data.accrued, 0);
        });
    }

    #[test]
    fn test_update_rz_emis_data_index_already_updated() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });
        let backstop_id = create_backstop(&e);
        let pool = Address::generate(&e);

        e.as_contract(&backstop_id, || {
            storage::set_rz_emission_index(&e, &22_00000000000000);
            storage::set_rz_emis_data(
                &e,
                &pool,
                &RzEmissionData {
                    index: 22_00000000000000,
                    accrued: 100_0000000,
                },
            );
            storage::set_pool_balance(
                &e,
                &pool,
                &PoolBalance {
                    shares: 150_0000000,
                    tokens: 0,
                    q4w: 2_0000000,
                },
            );

            let result = update_rz_emis_data(&e, &pool, false);
            let actual_data = storage::get_rz_emis_data(&e, &pool).unwrap_optimized();
            assert_eq!(result, 0);
            assert_eq!(actual_data.index, 22_00000000000000);
            assert_eq!(actual_data.accrued, 100_0000000);
        });
    }

    #[test]
    fn test_update_rz_emis_data_gulp_updated_index() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });
        let backstop_id = create_backstop(&e);
        let pool = Address::generate(&e);

        e.as_contract(&backstop_id, || {
            storage::set_rz_emission_index(&e, &22_00000000000000);
            storage::set_rz_emis_data(
                &e,
                &pool,
                &RzEmissionData {
                    index: 22_00000000000000,
                    accrued: 100_0000000,
                },
            );
            storage::set_pool_balance(
                &e,
                &pool,
                &PoolBalance {
                    shares: 150_0000000,
                    tokens: 0,
                    q4w: 2_0000000,
                },
            );
            let result = update_rz_emis_data(&e, &pool, true);
            let actual_data = storage::get_rz_emis_data(&e, &pool).unwrap_optimized();
            assert_eq!(result, 100_0000000);
            assert_eq!(actual_data.index, 22_00000000000000);
            assert_eq!(actual_data.accrued, 0);
        });
    }

    #[test]
    fn test_update_rz_emis_data_no_emis_data() {
        let e = Env::default();
        e.ledger().set(LedgerInfo {
            timestamp: BACKSTOP_EPOCH,
            protocol_version: 22,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });
        let backstop_id = create_backstop(&e);
        let pool = Address::generate(&e);

        e.as_contract(&backstop_id, || {
            storage::set_rz_emission_index(&e, &22_00000000000000);

            storage::set_pool_balance(
                &e,
                &pool,
                &PoolBalance {
                    shares: 150_0000000,
                    tokens: 0,
                    q4w: 2_0000000,
                },
            );
            let result = update_rz_emis_data(&e, &pool, false);
            let actual_data = storage::get_rz_emis_data(&e, &pool);
            assert_eq!(result, 0);
            assert!(actual_data.is_none());
        });
    }
}

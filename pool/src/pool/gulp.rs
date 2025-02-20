use sep_41_token::TokenClient;
use soroban_sdk::{Address, Env};

use crate::{constants::SCALAR_7, storage};

use super::Reserve;

/// Accrues unaccounted for tokens to the backstop credit so they aren't lost
///
/// ### Arguments
/// * `asset` - The address of the asset to gulp
///
/// ### Returns
/// * (i128, i128) - The token delta in the pool's asset balance and the reserve's B token supply, the additional backstop credit
pub fn execute_gulp(e: &Env, asset: &Address) -> (i128, i128) {
    let pool_config = storage::get_pool_config(e);
    let mut reserve = Reserve::load(e, &pool_config, asset);
    let pool_token_balance = TokenClient::new(e, asset).balance(&e.current_contract_address());
    let reserve_token_balance =
        reserve.total_supply(e) + reserve.data.backstop_credit - reserve.total_liabilities(e);
    let token_balance_delta = pool_token_balance - reserve_token_balance;
    if token_balance_delta == 0 {
        return (0, 0);
    }
    let old_backstop_credit = reserve.data.backstop_credit;
    // Input a bstop_rate of 100 here because we accrue all excess tokens to the backstop
    reserve.gulp(e, SCALAR_7 as u32, token_balance_delta);

    reserve.store(e);
    return (
        token_balance_delta,
        reserve.data.backstop_credit - old_backstop_credit,
    );
}

#[cfg(test)]
mod tests {
    use std::println;

    use crate::constants::SCALAR_7;
    use crate::pool::execute_gulp;
    use crate::storage::{self, PoolConfig};
    use crate::testutils;
    use soroban_sdk::{
        testutils::{Address as _, Ledger, LedgerInfo},
        Address, Env,
    };

    #[test]
    fn test_execute_gulp() {
        let e = Env::default();
        e.mock_all_auths();
        e.ledger().set(LedgerInfo {
            timestamp: 100,
            protocol_version: 22,
            sequence_number: 1234,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });
        let bombadil = Address::generate(&e);
        let pool = testutils::create_pool(&e);
        let (oracle, _) = testutils::create_mock_oracle(&e);

        let (underlying, underlying_client) = testutils::create_token_contract(&e, &bombadil);
        let (reserve_config, mut reserve_data) = testutils::default_reserve_meta();
        reserve_data.b_rate = 1_000_000_000_000;
        reserve_data.d_rate = 1_000_000_000_000;
        reserve_data.d_supply = 500 * SCALAR_7;
        reserve_data.b_supply = 1000 * SCALAR_7;
        reserve_data.backstop_credit = 500;
        reserve_data.last_time = 100;
        testutils::create_reserve(&e, &pool, &underlying, &reserve_config, &reserve_data);

        let additional_tokens = 10 * SCALAR_7;
        underlying_client.mint(&pool, &additional_tokens);
        e.as_contract(&pool, || {
            let pool_config = PoolConfig {
                oracle,
                min_collateral: 1_0000000,
                bstop_rate: 0_1000000,
                status: 0,
                max_positions: 4,
            };
            storage::set_pool_config(&e, &pool_config);

            let (token_delta_result, new_backstop_credit) = execute_gulp(&e, &underlying);
            assert_eq!(token_delta_result, additional_tokens);
            let new_reserve_data = storage::get_res_data(&e, &underlying);
            println!("new backstop credit: {}", new_reserve_data.backstop_credit);
            assert_eq!(new_backstop_credit, additional_tokens);

            let new_reserve_data = storage::get_res_data(&e, &underlying);
            assert_eq!(new_reserve_data.last_time, 100);
            assert_eq!(new_reserve_data.backstop_credit, additional_tokens + 500);
        });
    }

    #[test]
    fn test_execute_gulp_accrues_interest() {
        let e = Env::default();
        e.mock_all_auths();
        e.ledger().set(LedgerInfo {
            timestamp: 100,
            protocol_version: 22,
            sequence_number: 1234,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });
        let bombadil = Address::generate(&e);
        let pool = testutils::create_pool(&e);
        let (oracle, _) = testutils::create_mock_oracle(&e);

        let (underlying, underlying_client) = testutils::create_token_contract(&e, &bombadil);
        let (reserve_config, mut reserve_data) = testutils::default_reserve_meta();
        reserve_data.b_rate = 1_000_000_000_000;
        reserve_data.d_rate = 1_000_000_000_000;
        reserve_data.d_supply = 500 * SCALAR_7;
        reserve_data.b_supply = 1000 * SCALAR_7;
        reserve_data.backstop_credit = 500;
        reserve_data.last_time = 0;
        testutils::create_reserve(&e, &pool, &underlying, &reserve_config, &reserve_data);

        let additional_tokens = 10 * SCALAR_7;
        underlying_client.mint(&pool, &additional_tokens);
        e.as_contract(&pool, || {
            let pool_config = PoolConfig {
                oracle,
                min_collateral: 1_0000000,
                bstop_rate: 0_1000000,
                status: 0,
                max_positions: 4,
            };
            storage::set_pool_config(&e, &pool_config);

            let (token_delta_result, new_backstop_credit) = execute_gulp(&e, &underlying);
            assert_eq!(token_delta_result, additional_tokens);
            assert_eq!(new_backstop_credit, additional_tokens);

            let new_reserve_data = storage::get_res_data(&e, &underlying);
            assert_eq!(new_reserve_data.b_rate, 1_000_000_000_000 + 62000);
            assert_eq!(new_reserve_data.last_time, 100);
            assert_eq!(
                new_reserve_data.backstop_credit,
                additional_tokens + 500 + 68
            );
        });
    }

    #[test]
    fn test_execute_gulp_zero_delta_skips() {
        let e = Env::default();
        e.mock_all_auths_allowing_non_root_auth();
        e.ledger().set(LedgerInfo {
            timestamp: 100,
            protocol_version: 22,
            sequence_number: 1234,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });
        let bombadil = Address::generate(&e);
        let pool = testutils::create_pool(&e);
        let (oracle, _) = testutils::create_mock_oracle(&e);

        let (underlying, _) = testutils::create_token_contract(&e, &bombadil);
        let (reserve_config, mut reserve_data) = testutils::default_reserve_meta();
        reserve_data.b_rate = 1_000_000_000_000;
        reserve_data.d_rate = 1_000_000_000_000;
        reserve_data.d_supply = 500 * SCALAR_7;
        reserve_data.b_supply = 1000 * SCALAR_7;
        reserve_data.backstop_credit = 0;
        reserve_data.last_time = 0;
        testutils::create_reserve(&e, &pool, &underlying, &reserve_config, &reserve_data);

        e.as_contract(&pool, || {
            let pool_config = PoolConfig {
                oracle,
                min_collateral: 1_0000000,
                bstop_rate: 0_1000000,
                status: 0,
                max_positions: 4,
            };
            storage::set_pool_config(&e, &pool_config);

            let (token_delta_result, new_backstop_credit) = execute_gulp(&e, &underlying);
            assert_eq!(token_delta_result, 0);
            assert_eq!(new_backstop_credit, 0);

            // data not set
            let new_reserve_data = storage::get_res_data(&e, &underlying);
            assert_eq!(new_reserve_data.b_rate, 1_000_000_000_000);
            assert_eq!(new_reserve_data.last_time, 0);
            assert_eq!(new_reserve_data.backstop_credit, 0);
        });
    }
}

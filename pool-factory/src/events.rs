use soroban_sdk::{Address, Env, Symbol};

pub struct PoolFactoryEvents {}

impl PoolFactoryEvents {
    /// Emitted when a pool is deployed by the factory
    ///
    /// - topics - `["distribute"]`
    /// - data - `pool_: Address, distribution_amount: i128`
    ///
    /// ### Arguments
    /// * `pool_address` - The address of the pool
    /// * `distribution_amount` - The amount of tokens distributed
    pub fn deploy(e: &Env, pool_address: Address) {
        let topics = (Symbol::new(e, "deploy"),);
        e.events().publish(topics, pool_address);
    }
}

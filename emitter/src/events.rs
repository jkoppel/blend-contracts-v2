use soroban_sdk::{Address, Env, Symbol, Vec};

use crate::Swap;

pub struct EmitterEvents {}

impl EmitterEvents {
    /// Emitted when tokens are distributed to the backstop
    ///
    /// - topics - `["distribute"]`
    /// - data - `[backstop_address: Address, distribution_amount: i128]`
    ///
    /// ### Arguments
    /// * `backstop_address` - The address of the backstop
    /// * `distribution_amount` - The amount of tokens distributed
    pub fn distribute(e: &Env, backstop_address: Address, distribution_amount: i128) {
        let topics = (Symbol::new(e, "distribute"),);
        e.events()
            .publish(topics, (backstop_address, distribution_amount));
    }

    /// Emitted when a backstop swap is queued
    ///
    /// - topics - `["q_swap"]`
    /// - data - `swap: Swap`
    ///
    /// ### Arguments
    /// * `swap` - The backstop swap details
    pub fn q_swap(e: &Env, swap: Swap) {
        let topics = (Symbol::new(e, "q_swap"),);
        e.events().publish(topics, swap);
    }

    /// Emitted when a backstop swap is cancelled
    ///
    /// - topics - `["del_swap"]`
    /// - data - `swap: Swap`
    ///
    /// ### Arguments
    /// * `swap` - The backstop swap details
    pub fn del_swap(e: &Env, swap: Swap) {
        let topics = (Symbol::new(e, "del_swap"),);
        e.events().publish(topics, swap);
    }

    /// Emitted when a backstop swap is executed
    ///
    /// - topics - `["swap"]`
    /// - data - `swap: Swap`
    ///
    /// ### Arguments
    /// * `swap` - The backstop swap details
    pub fn swap(e: &Env, swap: Swap) {
        let topics = (Symbol::new(e, "swap"),);
        e.events().publish(topics, swap);
    }

    /// Emitted when tokens are dropped from the backstop
    ///
    /// - topics - `["drop"]`
    /// - data - `list: Vec<(Address, i128)>`
    ///
    /// ### Arguments
    /// * `list` - The list of tokens dropped
    pub fn drop(e: &Env, list: Vec<(Address, i128)>) {
        let topics = (Symbol::new(e, "drop"),);
        e.events().publish(topics, list);
    }
}

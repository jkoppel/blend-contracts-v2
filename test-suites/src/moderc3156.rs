use soroban_sdk::{testutils::Address as _, Address, Env};

mod emitter_contract {
    soroban_sdk::contractimport!(file = "../target/wasm32-unknown-unknown/optimized/moderc3156_example.wasm");
}

use moderc3156_example::{FlashLoanReceiverModifiedERC3156Client, FlashLoanReceiverModifiedERC3156};

pub fn create_flashloan_receiver<'a>(e: &Env, wasm: bool) -> (Address, FlashLoanReceiverModifiedERC3156Client<'a>) {
    let contract_id = Address::generate(e);
    if wasm {
        e.register_at(&contract_id, emitter_contract::WASM, ());
    } else {
        e.register_at(&contract_id, FlashLoanReceiverModifiedERC3156 {}, ());
    }
    (contract_id.clone(), FlashLoanReceiverModifiedERC3156Client::new(e, &contract_id))
}

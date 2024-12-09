use soroban_sdk::{testutils::Address as _, Address, Env};

mod emitter_contract {
    soroban_sdk::contractimport!(file = "../target/wasm32-unknown-unknown/optimized/emitter.wasm");
}

use emitter::{EmitterClient, EmitterContract};

pub fn create_emitter<'a>(e: &Env, wasm: bool) -> (Address, EmitterClient<'a>) {
    let contract_id = Address::generate(e);
    if wasm {
        e.register_at(&contract_id, emitter_contract::WASM, ());
    } else {
        e.register_at(&contract_id, EmitterContract {}, ());
    }
    (contract_id.clone(), EmitterClient::new(e, &contract_id))
}

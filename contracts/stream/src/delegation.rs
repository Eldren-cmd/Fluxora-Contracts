//! Delegation parameter validation for delegated-withdraw operations.

use soroban_sdk::Env;

use crate::{load_delegated_nonce, load_stream, ContractError};

/// Validate the delegation parameters for a delegated-withdraw call.
///
/// Checks, in order:
/// 1. `relayer_fee >= 0` — rejects negative fee parameters.
/// 2. `deadline >= env.ledger().timestamp()` — rejects expired signatures.
/// 3. `nonce == current_nonce(stream.recipient)` — rejects replays.
pub(crate) fn validate_delegation_params(
    env: &Env,
    stream_id: u64,
    nonce: u64,
    deadline: u64,
    relayer_fee: i128,
) -> Result<(), ContractError> {
    if relayer_fee < 0 {
        return Err(ContractError::InvalidParams);
    }

    if env.ledger().timestamp() > deadline {
        return Err(ContractError::SignatureDeadlineExpired);
    }

    let stream = load_stream(env, stream_id)?;
    let current_nonce = load_delegated_nonce(env, &stream.recipient);
    if nonce != current_nonce {
        return Err(ContractError::InvalidSignature);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::{FluxoraStream, FluxoraStreamClient, StreamKind};
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::Client as TokenClient,
        Address, Env,
    };

    fn setup() -> (Env, FluxoraStreamClient<'static>, u64, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, FluxoraStream);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
        let admin = Address::generate(&env);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let client = FluxoraStreamClient::new(&env, &contract_id);
        client.init(&token_id, &admin);

        let sac = soroban_sdk::token::StellarAssetClient::new(&env, &token_id);
        sac.mint(&sender, &10_000_i128);
        TokenClient::new(&env, &token_id).approve(&sender, &contract_id, &i128::MAX, &100_000);

        env.ledger().set_timestamp(0);
        let stream_id = client.create_stream(
            &sender,
            &recipient,
            &1000_i128,
            &1_i128,
            &0u64,
            &0u64,
            &1000u64,
            &0,
            &None,
            &StreamKind::Linear,
        );

        (env, client, stream_id, recipient)
    }

    #[test]
    fn test_valid_relayer_fee_passes() {
        let (env, client, stream_id, _recipient) = setup();
        env.ledger().set_timestamp(100);

        let result = env.as_contract(&client.address, || {
            validate_delegation_params(&env, stream_id, 0, 100, 10)
        });
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn test_negative_relayer_fee_fails() {
        let (env, client, stream_id, _recipient) = setup();
        env.ledger().set_timestamp(100);

        let result = env.as_contract(&client.address, || {
            validate_delegation_params(&env, stream_id, 0, 100, -1)
        });
        assert_eq!(result, Err(ContractError::InvalidParams));
    }
}
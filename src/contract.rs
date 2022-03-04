// src/contract.rs

use cosmwasm_std::{
    generic_err, log, to_binary, Api, Binary, Env, Extern, HandleResponse, HumanAddr, InitResponse,
    Querier, StdResult, Storage, Uint128,
};

use crate::msg::{BalanceResponse, ConfigResponse, HandleMsg, InitMsg, QueryMsg};
use crate::state::{balance_get, balance_set, config_get, config_set, Config};

use crate::deposit::{compute_exchange_rate_raw, deposit_stable, redeem_stable};

use cw20::{Cw20Coin, Cw20ReceiveMsg, MinterResponse};
use moneymarket::common::optional_addr_validate;
use moneymarket::interest_model::BorrowRateResponse;
use moneymarket::market::{
    ConfigResponse, Cw20HookMsg, EpochStateResponse, ExecuteMsg, InstantiateMsg, QueryMsg,
    StateResponse,
};

pub fn init<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    msg: InitMsg,
) -> StdResult<InitResponse> {
    // Initial balances
    for row in msg.initial_balances {
        let address = deps.api.canonical_address(&row.address)?;
        balance_set(&mut deps.storage, &address, &row.amount)?;
    }
    config_set(
        &mut deps.storage,
        &Config {
            name: msg.name,
            symbol: msg.symbol,
            owner: env.message.sender,
        },
    )?;

    store_config(
        deps.storage,
        &Config {
            contract_addr: deps.api.addr_canonicalize(env.contract.address.as_str())?,
            owner_addr: deps.api.addr_canonicalize(&msg.owner_addr)?,
            aterra_contract: CanonicalAddr::from(vec![]),
            overseer_contract: CanonicalAddr::from(vec![]),
            interest_model: CanonicalAddr::from(vec![]),
            distribution_model: CanonicalAddr::from(vec![]),
            collector_contract: CanonicalAddr::from(vec![]),
            distributor_contract: CanonicalAddr::from(vec![]),
            stable_denom: msg.stable_denom.clone(),
            max_borrow_factor: msg.max_borrow_factor,
        },
    )?;

    store_state(
        deps.storage,
        &State {
            total_liabilities: Decimal256::zero(),
            total_reserves: Decimal256::zero(),
            last_interest_updated: env.block.height,
            last_reward_updated: env.block.height,
            global_interest_index: Decimal256::one(),
            global_reward_index: Decimal256::zero(),
            anc_emission_rate: msg.anc_emission_rate,
            prev_aterra_supply: Uint256::zero(),
            prev_exchange_rate: Decimal256::one(),
        },
    )?;

    Ok(
        Response::new().add_submessages(vec![SubMsg::reply_on_success(
            CosmosMsg::Wasm(WasmMsg::Instantiate {
                admin: None,
                code_id: msg.aterra_code_id,
                funds: vec![],
                label: "".to_string(),
                msg: to_binary(&TokenInstantiateMsg {
                    name: format!("Anchor Terra {}", msg.stable_denom[1..].to_uppercase()),
                    symbol: format!(
                        "a{}T",
                        msg.stable_denom[1..(msg.stable_denom.len() - 1)].to_uppercase()
                    ),
                    decimals: 6u8,
                    initial_balances: vec![Cw20Coin {
                        address: env.contract.address.to_string(),
                        amount: Uint128::from(INITIAL_DEPOSIT_AMOUNT),
                    }],
                    mint: Some(MinterResponse {
                        minter: env.contract.address.to_string(),
                        cap: None,
                    }),
                })?,
            }),
            1,
        )]),
    )
}


#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Receive(msg) => receive_cw20(deps, env, info, msg),
        ExecuteMsg::RegisterContracts {
            overseer_contract,
            interest_model,
            distribution_model,
            collector_contract,
            distributor_contract,
        } => {
            let api = deps.api;
            register_contracts(
                deps,
                api.addr_validate(&overseer_contract)?,
                api.addr_validate(&interest_model)?,
                api.addr_validate(&distribution_model)?,
                api.addr_validate(&collector_contract)?,
                api.addr_validate(&distributor_contract)?,
            )
        }
        ExecuteMsg::UpdateConfig {
            owner_addr,
            interest_model,
            distribution_model,
            max_borrow_factor,
        } => {
            let api = deps.api;
            update_config(
                deps,
                env,
                info,
                optional_addr_validate(api, owner_addr)?,
                optional_addr_validate(api, interest_model)?,
                optional_addr_validate(api, distribution_model)?,
                max_borrow_factor,
            )
        }
        ExecuteMsg::ExecuteEpochOperations {
            deposit_rate,
            target_deposit_rate,
            threshold_deposit_rate,
            distributed_interest,
        } => execute_epoch_operations(
            deps,
            env,
            info,
            deposit_rate,
            target_deposit_rate,
            threshold_deposit_rate,
            distributed_interest,
        ),
        ExecuteMsg::DepositStable {} => deposit_stable(deps, env, info),
        // ExecuteMsg::BorrowStable { borrow_amount, to } => {
        //     let api = deps.api;
        //     borrow_stable(
        //         deps,
        //         env,
        //         info,
        //         borrow_amount,
        //         optional_addr_validate(api, to)?,
        //     )
        // }
        ExecuteMsg::RepayStable {} => repay_stable(deps, env, info),
        ExecuteMsg::RepayStableFromLiquidation {
            borrower,
            prev_balance,
        } => {
            let api = deps.api;
            repay_stable_from_liquidation(
                deps,
                env,
                info,
                api.addr_validate(&borrower)?,
                prev_balance,
            )
        }
        // ExecuteMsg::ClaimRewards { to } => {
        //     let api = deps.api;
        //     claim_rewards(deps, env, info, optional_addr_validate(api, to)?)
        // }
    }
}

pub fn receive_cw20(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    let contract_addr = info.sender;
    match from_binary(&cw20_msg.msg) {
        Ok(Cw20HookMsg::RedeemStable {}) => {
            // only asset contract can execute this message
            let config: Config = read_config(deps.storage)?;
            if deps.api.addr_canonicalize(contract_addr.as_str())? != config.aterra_contract {
                return Err(ContractError::Unauthorized {});
            }

            let cw20_sender_addr = deps.api.addr_validate(&cw20_msg.sender)?;
            redeem_stable(deps, env, cw20_sender_addr, cw20_msg.amount)
        }
        _ => Err(ContractError::MissingRedeemStableHook {}),
    }
}

pub fn query<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    msg: QueryMsg,
) -> StdResult<Binary> {
    match msg {
        QueryMsg::Balance { address } => {
            let address = deps.api.canonical_address(&address)?;
            let balance = balance_get(&deps.storage, &address);
            let out = to_binary(&BalanceResponse { balance })?;
            Ok(out)
        }
        QueryMsg::Config {} => {
            let config = config_get(&deps.storage)?;
            let out = to_binary(&ConfigResponse {
                name: config.name,
                symbol: config.symbol,
                owner: deps.api.human_address(&config.owner)?,
            })?;
            Ok(out)
        }
    }
}

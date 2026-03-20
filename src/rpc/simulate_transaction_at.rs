use alloy_consensus::{transaction::SignerRecoverable, BlockHeader as _, Transaction as _};
use alloy_eips::{BlockId, Decodable2718};
use alloy_evm::Evm;
use alloy_primitives::U256;
use alloy_rpc_types::{Log, TransactionRequest};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
    types::ErrorObjectOwned,
};
use jsonrpsee_types::error::INVALID_PARAMS_CODE;
use reth::rpc::server_types::eth::{error::api::FromEvmHalt, simulate, EthApiError, RevertError};
use reth_evm::{block::BlockExecutor, ConfigureEvm};
use reth_primitives::TransactionSigned;
use reth_primitives_traits::{Recovered, SignedTransaction as _};
use reth_provider::BlockIdReader;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_rpc_eth_api::{helpers::FullEthApi, EthApiTypes, FromEthApiError, RpcNodeCore};
use revm::context_interface::result::ExecutionResult;

use crate::rpc::tx_input::SimulationTxInput;

/// Result status for `eth_simulateTransactionAt`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SimulateTransactionAtStatus {
    Success,
    Revert,
    Halt,
}

/// Result payload for `eth_simulateTransactionAt`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulateTransactionAtResult {
    pub status: SimulateTransactionAtStatus,
    pub gas_used: U256,
    pub logs: Vec<Log>,
    pub return_data: alloy_primitives::Bytes,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthSimulateTransactionAtApi {
    #[method(name = "simulateTransactionAt")]
    async fn simulate_transaction_at(
        &self,
        block_id: BlockId,
        transaction_index: u64,
        transaction: SimulationTxInput<TransactionRequest>,
    ) -> RpcResult<SimulateTransactionAtResult>;
}

#[derive(Debug, Clone)]
pub struct EthSimulateTransactionAtApiImpl<Eth: EthApiTypes> {
    eth_api: Eth,
}

impl<Eth: EthApiTypes> EthSimulateTransactionAtApiImpl<Eth> {
    pub const fn new(eth_api: Eth) -> Self {
        Self { eth_api }
    }
}

#[async_trait]
impl<Eth> EthSimulateTransactionAtApiServer for EthSimulateTransactionAtApiImpl<Eth>
where
    Eth: FullEthApi<NetworkTypes = alloy_network::Ethereum> + Send + Sync + 'static,
    jsonrpsee_types::error::ErrorObject<'static>: From<Eth::Error>,
{
    async fn simulate_transaction_at(
        &self,
        block_id: BlockId,
        transaction_index: u64,
        transaction: SimulationTxInput<TransactionRequest>,
    ) -> RpcResult<SimulateTransactionAtResult> {
        let mut target_block = block_id;
        if !target_block.is_pending() {
            target_block = self
                .eth_api
                .provider()
                .block_hash_for_id(target_block)
                .map_err(|_| EthApiError::HeaderNotFound(target_block))?
                .ok_or_else(|| EthApiError::HeaderNotFound(target_block))?
                .into();
        }

        let (evm_env, _) = self.eth_api.evm_env_at(target_block).await?;
        let block = self
            .eth_api
            .recovered_block(target_block)
            .await?
            .ok_or(EthApiError::HeaderNotFound(block_id))?;
        let tx_count = block.transactions_recovered().count() as u64;
        if transaction_index > tx_count {
            return Err(ErrorObjectOwned::owned(
                INVALID_PARAMS_CODE,
                format!(
                    "transactionIndex {transaction_index} exceeds block transaction count {tx_count}"
                ),
                None::<()>,
            ));
        }

        let tx_index = transaction_index as usize;
        let request = decode_candidate_transaction(transaction)?;
        let eth_api = self.eth_api.clone();
        self.eth_api
            .spawn_with_state_at_block(block.parent_hash().into(), move |state| {
                let mut db =
                    State::builder().with_database(StateProviderDatabase::new(state)).build();

                let mut evm_env = evm_env;
                evm_env.cfg_env.disable_block_gas_limit = true;
                evm_env.block_env.gas_limit = u64::MAX;
                let block_base_fee = evm_env.block_env.basefee;
                let chain_id = evm_env.cfg_env.chain_id;

                let evm = RpcNodeCore::evm_config(&eth_api).evm_with_env(&mut db, evm_env);
                let ctx = RpcNodeCore::evm_config(&eth_api).context_for_block(block.sealed_block());
                let mut executor = RpcNodeCore::evm_config(&eth_api).create_executor(evm, ctx);
                executor.apply_pre_execution_changes().map_err(Eth::Error::from_eth_err)?;

                let mut gas_used_so_far = 0_u64;
                for tx in block.transactions_recovered().take(tx_index) {
                    let gas_used =
                        executor.execute_transaction(tx).map_err(Eth::Error::from_eth_err)?;
                    gas_used_so_far = gas_used_so_far.saturating_add(gas_used);
                }

                let remaining_block_gas = block.gas_limit().saturating_sub(gas_used_so_far);
                let default_gas_limit = if eth_api.call_gas_limit() > 0 {
                    remaining_block_gas.min(eth_api.call_gas_limit())
                } else {
                    remaining_block_gas
                };

                let tx = match simulate::resolve_transaction(
                    request,
                    default_gas_limit,
                    block_base_fee,
                    chain_id,
                    executor.evm_mut().db_mut(),
                    eth_api.tx_resp_builder(),
                ) {
                    Ok(tx) => tx,
                    Err(err) => {
                        return Ok(SimulateTransactionAtResult {
                            status: SimulateTransactionAtStatus::Halt,
                            gas_used: U256::ZERO,
                            logs: Vec::new(),
                            return_data: alloy_primitives::Bytes::new(),
                            error: Some(err.to_string()),
                        })
                    }
                };

                let tx_hash = *tx.inner().tx_hash();
                let tx_gas_limit = tx.inner().gas_limit();
                let mut execution_result = None;
                if let Err(err) = executor.execute_transaction_with_result_closure(tx, |result| {
                    execution_result = Some(result.clone())
                }) {
                    return Ok(SimulateTransactionAtResult {
                        status: SimulateTransactionAtStatus::Halt,
                        gas_used: U256::ZERO,
                        logs: Vec::new(),
                        return_data: alloy_primitives::Bytes::new(),
                        error: Some(err.to_string()),
                    });
                }

                let Some(result) = execution_result else {
                    return Ok(SimulateTransactionAtResult {
                        status: SimulateTransactionAtStatus::Halt,
                        gas_used: U256::ZERO,
                        logs: Vec::new(),
                        return_data: alloy_primitives::Bytes::new(),
                        error: Some(
                            "missing execution result for candidate transaction".to_string(),
                        ),
                    });
                };

                let tx_index = tx_index as u64;
                let block_number = block.number();
                let block_timestamp = block.timestamp();

                let simulated = match result {
                    ExecutionResult::Success { output, gas_used, logs, .. } => {
                        let logs = logs
                            .into_iter()
                            .map(|log| Log {
                                inner: log,
                                transaction_hash: Some(tx_hash),
                                transaction_index: Some(tx_index),
                                block_number: Some(block_number),
                                block_timestamp: Some(block_timestamp),
                                ..Default::default()
                            })
                            .collect();
                        SimulateTransactionAtResult {
                            status: SimulateTransactionAtStatus::Success,
                            gas_used: U256::from(gas_used),
                            logs,
                            return_data: output.into_data(),
                            error: None,
                        }
                    }
                    ExecutionResult::Revert { output, gas_used } => SimulateTransactionAtResult {
                        status: SimulateTransactionAtStatus::Revert,
                        gas_used: U256::from(gas_used),
                        logs: Vec::new(),
                        return_data: output.clone(),
                        error: Some(normalize_error_message(
                            SimulateTransactionAtStatus::Revert,
                            RevertError::new(output).to_string(),
                        )),
                    },
                    ExecutionResult::Halt { reason, gas_used } => SimulateTransactionAtResult {
                        status: SimulateTransactionAtStatus::Halt,
                        gas_used: U256::from(gas_used),
                        logs: Vec::new(),
                        return_data: alloy_primitives::Bytes::new(),
                        error: Some(Eth::Error::from_evm_halt(reason, tx_gas_limit).to_string()),
                    },
                };

                Ok(simulated)
            })
            .await
            .map_err(Into::into)
    }
}

#[cfg(test)]
fn convert_sim_call_result(
    call: alloy_rpc_types::simulate::SimCallResult,
) -> SimulateTransactionAtResult {
    let status = if call.status {
        SimulateTransactionAtStatus::Success
    } else if call.error.as_ref().is_some_and(|err| err.code == 3) {
        SimulateTransactionAtStatus::Revert
    } else {
        SimulateTransactionAtStatus::Halt
    };

    let error = call.error.map(|err| normalize_error_message(status.clone(), err.message));

    SimulateTransactionAtResult {
        status,
        gas_used: U256::from(call.gas_used),
        logs: call.logs,
        return_data: call.return_data,
        error,
    }
}

pub(crate) fn decode_candidate_transaction(
    transaction: SimulationTxInput<TransactionRequest>,
) -> Result<TransactionRequest, ErrorObjectOwned> {
    match transaction {
        SimulationTxInput::Request(request) => Ok(request),
        SimulationTxInput::Raw { raw_tx } => {
            let tx = TransactionSigned::decode_2718_exact(raw_tx.as_ref()).map_err(|err| {
                ErrorObjectOwned::owned(
                    INVALID_PARAMS_CODE,
                    format!("invalid signed transaction: {err}"),
                    None::<()>,
                )
            })?;
            let sender = tx.recover_signer().map_err(|err| {
                ErrorObjectOwned::owned(
                    INVALID_PARAMS_CODE,
                    format!("invalid signer for signed transaction: {err}"),
                    None::<()>,
                )
            })?;

            Ok(TransactionRequest::from_recovered_transaction(Recovered::new_unchecked(tx, sender)))
        }
    }
}

pub(crate) fn normalize_error_message(
    status: SimulateTransactionAtStatus,
    message: String,
) -> String {
    if matches!(status, SimulateTransactionAtStatus::Revert) {
        if let Some(stripped) = message.strip_prefix("execution reverted: ") {
            return stripped.to_string();
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use alloy_rpc_types::simulate::{SimCallResult, SimulateError};

    #[test]
    fn maps_success_result() {
        let result = convert_sim_call_result(SimCallResult {
            return_data: Bytes::from_static(&[0xaa]),
            logs: Vec::new(),
            gas_used: 21_000,
            status: true,
            error: None,
        });

        assert_eq!(result.status, SimulateTransactionAtStatus::Success);
        assert_eq!(result.gas_used, U256::from(21_000));
        assert_eq!(result.return_data, Bytes::from_static(&[0xaa]));
        assert!(result.error.is_none());
    }

    #[test]
    fn maps_revert_result_and_strips_prefix() {
        let result = convert_sim_call_result(SimCallResult {
            return_data: Bytes::new(),
            logs: Vec::new(),
            gas_used: 42_000,
            status: false,
            error: Some(SimulateError {
                code: 3,
                message: "execution reverted: not enough balance".to_string(),
            }),
        });

        assert_eq!(result.status, SimulateTransactionAtStatus::Revert);
        assert_eq!(result.error.as_deref(), Some("not enough balance"));
    }

    #[test]
    fn maps_halt_result() {
        let result = convert_sim_call_result(SimCallResult {
            return_data: Bytes::new(),
            logs: Vec::new(),
            gas_used: 7_000,
            status: false,
            error: Some(SimulateError { code: -32_015, message: "out of gas".to_string() }),
        });

        assert_eq!(result.status, SimulateTransactionAtStatus::Halt);
        assert_eq!(result.error.as_deref(), Some("out of gas"));
    }

    #[test]
    fn accepts_unsigned_candidate_transaction() {
        let request = decode_candidate_transaction(SimulationTxInput::request(
            TransactionRequest::default().gas_limit(21_000),
        ))
        .expect("request input must pass through");

        assert_eq!(request.as_ref().gas, Some(21_000));
    }
}

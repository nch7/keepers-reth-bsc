use alloy_consensus::{BlockHeader, Transaction};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_evm::Evm;
use alloy_primitives::U256;
use alloy_rpc_types::{Log, TransactionRequest};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use reth::rpc::server_types::eth::{error::api::FromEvmHalt, simulate, EthApiError, RevertError};
use reth_evm::{block::BlockExecutor, ConfigureEvm};
use reth_primitives_traits::SignedTransaction;
use reth_provider::BlockIdReader;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_rpc_eth_api::{helpers::FullEthApi, EthApiTypes, FromEthApiError, RpcNodeCore};
use revm::context_interface::result::ExecutionResult;

use crate::rpc::{
    decode_candidate_transaction, normalize_error_message, SimulateTransactionAtResult,
    SimulateTransactionAtStatus, SimulationTxInput,
};

/// `keepers_` latest-state simulation RPC namespace.
#[cfg_attr(not(test), rpc(server, namespace = "keepers"))]
#[cfg_attr(test, rpc(server, client, namespace = "keepers"))]
pub trait KeepersApi {
    #[method(name = "simulateAtLatestState")]
    async fn simulate_at_latest_state(
        &self,
        bundle: Vec<SimulationTxInput<TransactionRequest>>,
    ) -> RpcResult<Vec<SimulateTransactionAtResult>>;
}

#[derive(Debug, Clone)]
pub struct KeepersApiImpl<Eth: EthApiTypes> {
    eth_api: Eth,
}

impl<Eth: EthApiTypes> KeepersApiImpl<Eth> {
    pub const fn new(eth_api: Eth) -> Self {
        Self { eth_api }
    }
}

#[async_trait]
impl<Eth> KeepersApiServer for KeepersApiImpl<Eth>
where
    Eth: FullEthApi<NetworkTypes = alloy_network::Ethereum> + Send + Sync + 'static,
    jsonrpsee_types::error::ErrorObject<'static>: From<Eth::Error>,
{
    async fn simulate_at_latest_state(
        &self,
        bundle: Vec<SimulationTxInput<TransactionRequest>>,
    ) -> RpcResult<Vec<SimulateTransactionAtResult>> {
        let calls =
            bundle.into_iter().map(decode_candidate_transaction).collect::<Result<Vec<_>, _>>()?;

        let latest_tag: BlockId = BlockNumberOrTag::Latest.into();
        let target_block: BlockId = self
            .eth_api
            .provider()
            .block_hash_for_id(latest_tag.clone())
            .map_err(|_| EthApiError::HeaderNotFound(latest_tag.clone()))?
            .ok_or_else(|| EthApiError::HeaderNotFound(latest_tag.clone()))?
            .into();

        let (mut evm_env, _) = self.eth_api.evm_env_at(target_block.clone()).await?;
        let block = self
            .eth_api
            .recovered_block(target_block.clone())
            .await?
            .ok_or(EthApiError::HeaderNotFound(latest_tag))?;

        let block_gas_limit = lenient_block_gas_limit(block.gas_limit(), &calls);
        let default_gas_limit = default_gas_limit(block_gas_limit, &calls);
        evm_env.cfg_env.disable_eip3607 = true;
        evm_env.cfg_env.disable_nonce_check = true;
        evm_env.cfg_env.disable_base_fee = true;
        evm_env.cfg_env.disable_block_gas_limit = true;
        evm_env.block_env.gas_limit = block_gas_limit;

        let block_base_fee = evm_env.block_env.basefee;
        let chain_id = evm_env.cfg_env.chain_id;
        let eth_api = self.eth_api.clone();

        self.eth_api
            .spawn_with_state_at_block(target_block, move |state| {
                let mut db =
                    State::builder().with_database(StateProviderDatabase::new(state)).build();
                let evm = RpcNodeCore::evm_config(&eth_api).evm_with_env(&mut db, evm_env);
                let ctx = RpcNodeCore::evm_config(&eth_api).context_for_block(block.sealed_block());
                let mut executor = RpcNodeCore::evm_config(&eth_api).create_executor(evm, ctx);
                executor.apply_pre_execution_changes().map_err(Eth::Error::from_eth_err)?;

                let block_number = block.number();
                let block_timestamp = block.timestamp();
                let mut outputs = Vec::with_capacity(calls.len());

                for (index, call) in calls.into_iter().enumerate() {
                    let tx = match simulate::resolve_transaction(
                        call,
                        default_gas_limit,
                        block_base_fee,
                        chain_id,
                        executor.evm_mut().db_mut(),
                        eth_api.tx_resp_builder(),
                    ) {
                        Ok(tx) => tx,
                        Err(err) => {
                            outputs.push(SimulateTransactionAtResult {
                                status: SimulateTransactionAtStatus::Halt,
                                gas_used: U256::ZERO,
                                logs: Vec::new(),
                                return_data: alloy_primitives::Bytes::new(),
                                error: Some(err.to_string()),
                            });
                            continue;
                        }
                    };

                    let tx_hash = *tx.inner().tx_hash();
                    let tx_gas_limit = tx.inner().gas_limit();
                    let mut execution_result = None;
                    if let Err(err) = executor
                        .execute_transaction_with_result_closure(tx, |result| {
                            execution_result = Some(result.clone())
                        })
                    {
                        outputs.push(SimulateTransactionAtResult {
                            status: SimulateTransactionAtStatus::Halt,
                            gas_used: U256::ZERO,
                            logs: Vec::new(),
                            return_data: alloy_primitives::Bytes::new(),
                            error: Some(err.to_string()),
                        });
                        continue;
                    }

                    let Some(result) = execution_result else {
                        outputs.push(SimulateTransactionAtResult {
                            status: SimulateTransactionAtStatus::Halt,
                            gas_used: U256::ZERO,
                            logs: Vec::new(),
                            return_data: alloy_primitives::Bytes::new(),
                            error: Some(
                                "missing execution result for bundle transaction".to_string(),
                            ),
                        });
                        continue;
                    };

                    outputs.push(match result {
                        ExecutionResult::Success { output, gas_used, logs, .. } => {
                            let logs = logs
                                .into_iter()
                                .map(|log| Log {
                                    inner: log,
                                    transaction_hash: Some(tx_hash),
                                    transaction_index: Some(index as u64),
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
                        ExecutionResult::Revert { output, gas_used } => {
                            let error = RevertError::new(output.clone()).to_string();
                            SimulateTransactionAtResult {
                                status: SimulateTransactionAtStatus::Revert,
                                gas_used: U256::from(gas_used),
                                logs: Vec::new(),
                                return_data: output,
                                error: Some(normalize_error_message(
                                    SimulateTransactionAtStatus::Revert,
                                    error,
                                )),
                            }
                        }
                        ExecutionResult::Halt { reason, gas_used } => SimulateTransactionAtResult {
                            status: SimulateTransactionAtStatus::Halt,
                            gas_used: U256::from(gas_used),
                            logs: Vec::new(),
                            return_data: alloy_primitives::Bytes::new(),
                            error: Some(
                                Eth::Error::from_evm_halt(reason, tx_gas_limit).to_string(),
                            ),
                        },
                    });
                }

                Ok(outputs)
            })
            .await
            .map_err(Into::into)
    }
}

fn lenient_block_gas_limit(block_gas_limit: u64, calls: &[TransactionRequest]) -> u64 {
    let total_specified_gas =
        calls.iter().filter_map(|call| call.as_ref().gas).fold(0_u64, u64::saturating_add);

    block_gas_limit.max(total_specified_gas)
}

fn default_gas_limit(block_gas_limit: u64, calls: &[TransactionRequest]) -> u64 {
    let total_specified_gas =
        calls.iter().filter_map(|call| call.as_ref().gas).fold(0_u64, u64::saturating_add);
    let txs_without_gas_limit = calls.iter().filter(|call| call.as_ref().gas.is_none()).count();

    if txs_without_gas_limit > 0 {
        (block_gas_limit.saturating_sub(total_specified_gas)) / txs_without_gas_limit as u64
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenient_block_gas_limit_respects_total_requested_gas() {
        let calls = vec![
            TransactionRequest::default().gas_limit(100),
            TransactionRequest::default().gas_limit(200),
        ];

        assert_eq!(lenient_block_gas_limit(250, &calls), 300);
        assert_eq!(lenient_block_gas_limit(400, &calls), 400);
    }

    #[test]
    fn default_gas_limit_splits_unspecified_gas_evenly() {
        let calls = vec![
            TransactionRequest::default().gas_limit(200),
            TransactionRequest::default(),
            TransactionRequest::default(),
        ];

        assert_eq!(default_gas_limit(1_000, &calls), 400);
    }
}

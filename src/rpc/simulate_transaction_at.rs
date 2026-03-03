use alloy_consensus::BlockHeader as _;
use alloy_eips::BlockId;
use alloy_primitives::U256;
use alloy_rpc_types::{
    simulate::{SimBlock, SimCallResult, SimulatePayload},
    BlockOverrides, BlockTransactions, Log, Transaction, TransactionRequest,
};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
    types::ErrorObjectOwned,
};
use jsonrpsee_types::error::{INTERNAL_ERROR_CODE, INVALID_PARAMS_CODE};
use reth::rpc::server_types::eth::EthApiError;
use reth_rpc_eth_api::{
    helpers::{EthBlocks, EthCall, FullEthApi},
    EthApiTypes,
};

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
        transaction: TransactionRequest,
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
        transaction: TransactionRequest,
    ) -> RpcResult<SimulateTransactionAtResult> {
        let block = EthBlocks::rpc_block(&self.eth_api, block_id, true)
            .await?
            .ok_or(EthApiError::HeaderNotFound(block_id))?;

        let txs = match &block.transactions {
            BlockTransactions::Full(txs) => txs,
            _ => {
                return Err(ErrorObjectOwned::owned(
                    INVALID_PARAMS_CODE,
                    "full transactions are required",
                    None::<()>,
                ));
            }
        };

        let tx_count = txs.len() as u64;
        if transaction_index > tx_count {
            return Err(ErrorObjectOwned::owned(
                INVALID_PARAMS_CODE,
                format!(
                    "transactionIndex {transaction_index} exceeds block transaction count {tx_count}"
                ),
                None::<()>,
            ));
        }

        let mut calls = txs
            .iter()
            .take(transaction_index as usize)
            .map(|tx: &Transaction| {
                TransactionRequest::from_transaction_with_sender(tx.clone(), tx.inner.signer())
            })
            .collect::<Vec<_>>();
        calls.push(transaction);

        let block_overrides = BlockOverrides {
            number: Some(U256::from(block.header.number())),
            difficulty: Some(block.header.difficulty()),
            time: Some(block.header.timestamp()),
            gas_limit: Some(block.header.gas_limit()),
            coinbase: Some(block.header.beneficiary()),
            random: block.header.mix_hash(),
            base_fee: block.header.base_fee_per_gas().map(U256::from),
            block_hash: None,
        };

        let payload = SimulatePayload::default()
            .extend(SimBlock::default().with_block_overrides(block_overrides).extend_calls(calls));

        let parent_block = block.header.parent_hash().into();
        let simulated = EthCall::simulate_v1(&self.eth_api, payload, Some(parent_block))
            .await
            .map_err(Into::into)?;
        let call = simulated
            .into_iter()
            .next()
            .and_then(|block| block.calls.into_iter().nth(transaction_index as usize))
            .ok_or_else(|| {
                ErrorObjectOwned::owned(
                    INTERNAL_ERROR_CODE,
                    "missing simulated call result",
                    None::<()>,
                )
            })?;

        Ok(convert_sim_call_result(call))
    }
}

fn convert_sim_call_result(call: SimCallResult) -> SimulateTransactionAtResult {
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

fn normalize_error_message(status: SimulateTransactionAtStatus, message: String) -> String {
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
    use alloy_rpc_types::simulate::SimulateError;

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
}

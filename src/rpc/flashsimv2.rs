use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::PayloadId;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use jsonrpsee_types::ErrorObjectOwned;

use crate::rpc::simulate_transaction_at::SimulateTransactionAtResult;
use crate::rpc::tx_input::SimulationTxInput;

const ERR_NOT_READY: i32 = -39_102;
const ERR_BAD_FILTER: i32 = -39_105;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilteredLogsParamsV1 {
    pub topic0s: Vec<B256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addresses: Option<Vec<Address>>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlashSimV2SimulateResponse {
    pub payload_id: PayloadId,
    pub index: u64,
    pub block_number: u64,
    pub generation: u64,
    pub duration_ms: u64,
    pub results: Vec<SimulateTransactionAtResult>,
}

#[cfg_attr(not(test), rpc(server, namespace = "flashsimv2"))]
#[cfg_attr(test, rpc(server, client, namespace = "flashsimv2"))]
pub trait FlashSimV2Api {
    #[method(name = "simulateV1")]
    async fn simulate_v1(
        &self,
        payload_id: PayloadId,
        index: u64,
        block_number: u64,
        sims: Vec<SimulationTxInput<alloy_rpc_types::TransactionRequest>>,
    ) -> RpcResult<FlashSimV2SimulateResponse>;

    #[method(name = "subscribeFilteredLogsV1")]
    async fn subscribe_filtered_logs_v1(&self, params: FilteredLogsParamsV1) -> RpcResult<String>;

    #[method(name = "unsubscribeFilteredLogsV1")]
    async fn unsubscribe_filtered_logs_v1(&self, subscription_id: String) -> RpcResult<bool>;

    #[method(name = "subscribeTxInclusionsV1")]
    async fn subscribe_tx_inclusions_v1(&self) -> RpcResult<String>;

    #[method(name = "unsubscribeTxInclusionsV1")]
    async fn unsubscribe_tx_inclusions_v1(&self, subscription_id: String) -> RpcResult<bool>;
}

#[derive(Debug, Clone, Default)]
pub struct FlashSimV2ApiImpl;

impl FlashSimV2ApiImpl {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl FlashSimV2ApiServer for FlashSimV2ApiImpl {
    async fn simulate_v1(
        &self,
        _payload_id: PayloadId,
        _index: u64,
        _block_number: u64,
        _sims: Vec<SimulationTxInput<alloy_rpc_types::TransactionRequest>>,
    ) -> RpcResult<FlashSimV2SimulateResponse> {
        Err(not_ready())
    }

    async fn subscribe_filtered_logs_v1(&self, params: FilteredLogsParamsV1) -> RpcResult<String> {
        if params.topic0s.is_empty() {
            return Err(bad_filter());
        }
        Err(not_ready())
    }

    async fn unsubscribe_filtered_logs_v1(&self, _subscription_id: String) -> RpcResult<bool> {
        Err(not_ready())
    }

    async fn subscribe_tx_inclusions_v1(&self) -> RpcResult<String> {
        Err(not_ready())
    }

    async fn unsubscribe_tx_inclusions_v1(&self, _subscription_id: String) -> RpcResult<bool> {
        Err(not_ready())
    }
}

fn not_ready() -> ErrorObjectOwned {
    ErrorObjectOwned::owned(ERR_NOT_READY, "flashblocks state is not available on BSC", None::<()>)
}

fn bad_filter() -> ErrorObjectOwned {
    ErrorObjectOwned::owned(ERR_BAD_FILTER, "topic0s must be non-empty", None::<()>)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, B256};

    use super::*;

    #[test]
    fn returns_expected_not_ready_code() {
        assert_eq!(not_ready().code(), ERR_NOT_READY);
    }

    #[test]
    fn returns_expected_bad_filter_code() {
        assert_eq!(bad_filter().code(), ERR_BAD_FILTER);
    }

    #[tokio::test]
    async fn simulate_v1_returns_not_ready() {
        let api = FlashSimV2ApiImpl::new();
        let err = api
            .simulate_v1(PayloadId::new([0; 8]), 0, 0, Vec::new())
            .await
            .expect_err("expected not-ready error");
        assert_eq!(err.code(), ERR_NOT_READY);
    }

    #[tokio::test]
    async fn subscribe_filtered_logs_v1_keeps_bad_filter_behavior() {
        let api = FlashSimV2ApiImpl::new();
        let err = api
            .subscribe_filtered_logs_v1(FilteredLogsParamsV1 {
                topic0s: Vec::new(),
                addresses: None,
            })
            .await
            .expect_err("expected bad-filter error");
        assert_eq!(err.code(), ERR_BAD_FILTER);
    }

    #[tokio::test]
    async fn subscribe_filtered_logs_v1_returns_not_ready_for_valid_filter() {
        let api = FlashSimV2ApiImpl::new();
        let err = api
            .subscribe_filtered_logs_v1(FilteredLogsParamsV1 {
                topic0s: vec![B256::ZERO],
                addresses: Some(vec![address!("0000000000000000000000000000000000000001")]),
            })
            .await
            .expect_err("expected not-ready error");
        assert_eq!(err.code(), ERR_NOT_READY);
    }
}

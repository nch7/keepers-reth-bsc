use alloy_primitives::Bytes;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Transaction input accepted by BSC simulation endpoints.
#[derive(Debug, Clone, PartialEq)]
pub enum SimulationTxInput<TxReq> {
    /// Raw signed transaction bytes.
    Raw {
        /// Signed EIP-2718-encoded transaction bytes.
        raw_tx: Bytes,
    },
    /// Unsigned transaction request fields.
    Request(TxReq),
}

impl<TxReq> SimulationTxInput<TxReq> {
    /// Creates a raw transaction input.
    pub fn raw(raw_tx: Bytes) -> Self {
        Self::Raw { raw_tx }
    }

    /// Creates a request input.
    pub fn request(request: TxReq) -> Self {
        Self::Request(request)
    }
}

impl<TxReq> Serialize for SimulationTxInput<TxReq>
where
    TxReq: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Raw { raw_tx } => {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct RawTxInput<'a> {
                    raw_tx: &'a Bytes,
                }

                RawTxInput { raw_tx }.serialize(serializer)
            }
            Self::Request(request) => request.serialize(serializer),
        }
    }
}

impl<'de, TxReq> Deserialize<'de> for SimulationTxInput<TxReq>
where
    TxReq: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;

        match value {
            serde_json::Value::Object(mut object) => {
                if let Some(raw_tx) = object.remove("rawTx") {
                    if !object.is_empty() {
                        return Err(serde::de::Error::custom(
                            "rawTx cannot be combined with transaction request fields",
                        ));
                    }

                    return Bytes::deserialize(raw_tx)
                        .map(Self::raw)
                        .map_err(serde::de::Error::custom);
                }

                TxReq::deserialize(serde_json::Value::Object(object))
                    .map(Self::request)
                    .map_err(serde::de::Error::custom)
            }
            other => TxReq::deserialize(other).map(Self::request).map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SimulationTxInput;
    use alloy_primitives::bytes;
    use alloy_rpc_types::TransactionRequest;

    #[test]
    fn deserializes_raw_tx_object() {
        let input: SimulationTxInput<TransactionRequest> =
            serde_json::from_value(serde_json::json!({ "rawTx": "0x1234" })).unwrap();

        assert_eq!(input, SimulationTxInput::raw(bytes!("0x1234")));
    }

    #[test]
    fn deserializes_unsigned_request_object() {
        let input: SimulationTxInput<TransactionRequest> =
            serde_json::from_value(serde_json::json!({
                "from": "0x0000000000000000000000000000000000000001",
                "to": "0x0000000000000000000000000000000000000002",
                "gas": "0x5208"
            }))
            .unwrap();

        match input {
            SimulationTxInput::Request(request) => {
                assert_eq!(request.as_ref().gas, Some(21_000));
                assert!(request.as_ref().to.is_some());
            }
            other => panic!("expected request input, got {other:?}"),
        }
    }
}

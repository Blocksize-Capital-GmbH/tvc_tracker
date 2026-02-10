use serde::{Deserialize, Deserializer};

/// Vote tower entry from parsed vote account
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoteLockout {
    pub slot: u64,
    pub confirmation_count: u32,
    /// Vote latency in slots (1 = fastest = 16 credits)
    #[serde(default)]
    pub latency: Option<u32>,
}

/// Epoch credits entry from parsed vote account
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpochCreditsEntry {
    pub epoch: u64,
    #[serde(deserialize_with = "string_or_u64")]
    pub credits: u64,
    #[serde(deserialize_with = "string_or_u64")]
    pub previous_credits: u64,
}

/// Deserialize a value that could be either a string or a number
fn string_or_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrU64 {
        String(String),
        U64(u64),
    }

    match StringOrU64::deserialize(deserializer)? {
        StringOrU64::String(s) => s.parse().map_err(serde::de::Error::custom),
        StringOrU64::U64(n) => Ok(n),
    }
}

/// Parsed vote account info
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoteAccountInfo {
    pub votes: Vec<VoteLockout>,
    pub root_slot: Option<u64>,
    pub epoch_credits: Vec<EpochCreditsEntry>,
}

/// Parsed vote account data wrapper
#[derive(Debug, Clone, Deserialize)]
pub struct ParsedVoteData {
    pub info: VoteAccountInfo,
    #[serde(rename = "type")]
    pub account_type: String,
}

/// Account data with parsed encoding
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AccountData {
    Parsed {
        parsed: ParsedVoteData,
        program: String,
    },
    Raw(String, String),
}

/// Account value in notification
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountValue {
    pub lamports: u64,
    pub data: AccountData,
    pub owner: String,
    pub executable: bool,
    pub rent_epoch: u64,
}

/// Context for RPC response
#[derive(Debug, Clone, Deserialize)]
pub struct RpcContext {
    pub slot: u64,
}

/// Result wrapper with context
#[derive(Debug, Clone, Deserialize)]
pub struct RpcResult<T> {
    pub context: RpcContext,
    pub value: T,
}

/// WebSocket notification params
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationParams {
    pub result: RpcResult<AccountValue>,
    pub subscription: u64,
}

/// WebSocket message types
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum WsMessage {
    Notification {
        jsonrpc: String,
        method: String,
        params: NotificationParams,
    },
    SubscriptionResult {
        jsonrpc: String,
        result: u64,
        id: u64,
    },
    Error {
        jsonrpc: String,
        error: WsError,
        id: u64,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct WsError {
    pub code: i64,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_epoch_credits_string_parsing() {
        // Testnet returns credits as strings
        let json = r#"{
            "epoch": 909,
            "credits": "1173980518",
            "previousCredits": "1172127483"
        }"#;

        let entry: EpochCreditsEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.epoch, 909);
        assert_eq!(entry.credits, 1173980518);
        assert_eq!(entry.previous_credits, 1172127483);
    }

    #[test]
    fn test_epoch_credits_number_parsing() {
        // Some RPCs return credits as numbers
        let json = r#"{
            "epoch": 100,
            "credits": 500000,
            "previousCredits": 400000
        }"#;

        let entry: EpochCreditsEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.epoch, 100);
        assert_eq!(entry.credits, 500000);
        assert_eq!(entry.previous_credits, 400000);
    }

    #[test]
    fn test_vote_lockout_with_latency() {
        // Testnet includes latency field
        let json = r#"{
            "confirmationCount": 31,
            "latency": 1,
            "slot": 387331602
        }"#;

        let vote: VoteLockout = serde_json::from_str(json).unwrap();
        assert_eq!(vote.slot, 387331602);
        assert_eq!(vote.confirmation_count, 31);
        assert_eq!(vote.latency, Some(1));
    }

    #[test]
    fn test_vote_lockout_without_latency() {
        // Mainnet may not include latency field
        let json = r#"{
            "confirmationCount": 15,
            "slot": 12345678
        }"#;

        let vote: VoteLockout = serde_json::from_str(json).unwrap();
        assert_eq!(vote.slot, 12345678);
        assert_eq!(vote.confirmation_count, 15);
        assert_eq!(vote.latency, None);
    }

    #[test]
    fn test_vote_account_info_parsing() {
        let json = r#"{
            "votes": [
                {"slot": 100, "confirmationCount": 31, "latency": 1},
                {"slot": 101, "confirmationCount": 30, "latency": 2}
            ],
            "rootSlot": 99,
            "epochCredits": [
                {"epoch": 10, "credits": "1000", "previousCredits": "500"}
            ]
        }"#;

        let info: VoteAccountInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.votes.len(), 2);
        assert_eq!(info.votes[0].latency, Some(1));
        assert_eq!(info.votes[1].latency, Some(2));
        assert_eq!(info.root_slot, Some(99));
        assert_eq!(info.epoch_credits[0].credits, 1000);
    }

    #[test]
    fn test_ws_subscription_result() {
        let json = r#"{
            "jsonrpc": "2.0",
            "result": 12345,
            "id": 1
        }"#;

        let msg: WsMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsMessage::SubscriptionResult { result, id, .. } => {
                assert_eq!(result, 12345);
                assert_eq!(id, 1);
            }
            _ => panic!("Expected SubscriptionResult"),
        }
    }

    #[test]
    fn test_ws_error() {
        let json = r#"{
            "jsonrpc": "2.0",
            "error": {"code": -32601, "message": "Method not found"},
            "id": 1
        }"#;

        let msg: WsMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsMessage::Error { error, .. } => {
                assert_eq!(error.code, -32601);
                assert_eq!(error.message, "Method not found");
            }
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn test_full_notification_parsing() {
        // Realistic notification from testnet
        let json = r#"{
            "jsonrpc": "2.0",
            "method": "accountNotification",
            "params": {
                "result": {
                    "context": {"slot": 387331601},
                    "value": {
                        "lamports": 539430141643,
                        "data": {
                            "program": "vote",
                            "parsed": {
                                "info": {
                                    "votes": [
                                        {"confirmationCount": 31, "latency": 1, "slot": 387331570},
                                        {"confirmationCount": 30, "latency": 1, "slot": 387331571}
                                    ],
                                    "rootSlot": 387331569,
                                    "epochCredits": [
                                        {"credits": "1000", "epoch": 908, "previousCredits": "500"}
                                    ]
                                },
                                "type": "vote"
                            }
                        },
                        "owner": "Vote111111111111111111111111111111111111111",
                        "executable": false,
                        "rentEpoch": 0
                    }
                },
                "subscription": 13086
            }
        }"#;

        let msg: WsMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsMessage::Notification { params, .. } => {
                assert_eq!(params.result.context.slot, 387331601);
                assert_eq!(params.subscription, 13086);

                if let AccountData::Parsed { parsed, program } = &params.result.value.data {
                    assert_eq!(program, "vote");
                    assert_eq!(parsed.info.votes.len(), 2);
                    assert_eq!(parsed.info.votes[0].latency, Some(1));
                    assert_eq!(parsed.info.epoch_credits[0].credits, 1000);
                } else {
                    panic!("Expected Parsed data");
                }
            }
            _ => panic!("Expected Notification"),
        }
    }
}

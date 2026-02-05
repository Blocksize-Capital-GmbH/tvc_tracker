use serde::{Deserialize, Serialize};

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct VoteAccount {
    pub vote_pubkey: String,
    pub node_pubkey: String,
    pub activated_stake: u64,
    pub commission: u8,
    pub epoch_credits: Vec<[u64; 3]>, // [epoch, credits, prevCredits]
    pub last_vote: u64,
    pub root_slot: u64,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EpochInfo {
    pub epoch: u64,
    pub slot_index: u64,
    pub slots_in_epoch: u64,
    pub absolute_slot: u64,
}

#[derive(Serialize)]
pub struct RpcRequest<'a, T> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    pub params: T,
}

#[derive(Deserialize, Debug)]
pub struct RpcResponse<T> {
    pub jsonrpc: String,
    pub id: u64,
    pub result: Option<T>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Deserialize, Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Deserialize, Debug)]
pub struct GetVoteAccountsResult {
    pub current: Vec<VoteAccount>,
    pub delinquent: Vec<VoteAccount>,
}

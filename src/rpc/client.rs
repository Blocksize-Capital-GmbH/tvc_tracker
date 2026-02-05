use crate::config::Commitment;
use crate::rpc::types::{EpochInfo, GetVoteAccountsResult, VoteAccount};
use anyhow::{Context, anyhow};
use std::time::Duration;
use tracing::info;

#[allow(async_fn_in_trait)] // avoids the lint noise for internal projects
pub trait RpcClient {
    fn rpc_url(&self) -> &str;

    async fn get_epoch_info(&self, commitment: Commitment) -> anyhow::Result<EpochInfo>;

    async fn get_vote_account(
        &self,
        vote_pubkey: &str,
        commitment: Commitment,
    ) -> anyhow::Result<VoteAccount>;
}

pub struct HttpRpcClient {
    pub http: reqwest::Client,
    pub rpc_url: String,
}

impl HttpRpcClient {
    pub fn new(http: reqwest::Client, rpc_url: String) -> Self {
        Self { http, rpc_url }
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
        id: u64,
    ) -> anyhow::Result<T> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let raw = self.rpc_post_json(req).await?;
        let parsed: crate::rpc::types::RpcResponse<T> = serde_json::from_value(raw)?;
        if let Some(e) = parsed.error {
            anyhow::bail!("RPC error {}: {}", e.code, e.message);
        }
        parsed
            .result
            .ok_or_else(|| anyhow::anyhow!("missing result"))
    }

    async fn rpc_post_json(&self, body: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let mut attempt: u32 = 0;
        let mut backoff = Duration::from_millis(250);
        let max_attempts = 6;

        loop {
            attempt += 1;

            let resp = self.http.post(&self.rpc_url).json(&body).send().await;

            match resp {
                Ok(resp) => {
                    // Handle 429 rate limiting
                    if resp.status() == 429 {
                        let wait = resp
                            .headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(1);
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        continue;
                    }

                    // Handle 5xx with retry
                    if resp.status().is_server_error() {
                        if attempt >= max_attempts {
                            let status = resp.status();
                            let text = resp.text().await.unwrap_or_default();
                            return Err(anyhow!(
                                "RPC {0} server error {status}: {text}",
                                self.rpc_url
                            ));
                        }
                        tokio::time::sleep(backoff).await;
                        backoff = std::cmp::min(backoff * 2, Duration::from_secs(5));
                        continue;
                    }

                    let status = resp.status();
                    let v: serde_json::Value = resp.json().await.context("failed to parse JSON")?;
                    if !status.is_success() {
                        return Err(anyhow!("HTTP status {status}: {v}"));
                    }
                    return Ok(v);
                }

                Err(e) => {
                    // Retry only on transient-ish failures
                    let retryable = e.is_timeout() || e.is_connect() || e.is_request();
                    if !retryable || attempt >= max_attempts {
                        return Err(anyhow!("HTTP POST to {0} failed", self.rpc_url).context(e));
                    }
                    tracing::warn!(
                        "RPC request failed (attempt {attempt}/{max_attempts}): {e}. Retrying in {backoff:?}"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, Duration::from_secs(5));
                }
            }
        }
    }
}

impl RpcClient for HttpRpcClient {
    fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    async fn get_epoch_info(&self, commitment: Commitment) -> anyhow::Result<EpochInfo> {
        let params = serde_json::json!([{ "commitment": commitment }]);
        self.call("getEpochInfo", params, 2).await
    }

    async fn get_vote_account(
        &self,
        vote_pubkey: &str,
        commitment: Commitment,
    ) -> anyhow::Result<VoteAccount> {
        let params = serde_json::json!([{
            "commitment" : commitment,
            "votePubkey" : vote_pubkey,
        }]);

        let res: GetVoteAccountsResult = self.call("getVoteAccounts", params, 1).await?;

        // Entry could theoretically be in current or delinquent.
        let acct = res
            .current
            .into_iter()
            .chain(res.delinquent.into_iter())
            .find(|a| a.vote_pubkey == vote_pubkey)
            .ok_or_else(|| anyhow!("vote pubkey not found in getVoteAccounts response"))?;

        info!(
            "node={} stake={} commission={} last_vote={} root_slot={}",
            acct.node_pubkey, acct.activated_stake, acct.commission, acct.last_vote, acct.root_slot
        );

        Ok(acct.clone())
    }
}

pub struct FakeRpc {
    epoch: EpochInfo,
    acct: VoteAccount,
}

impl RpcClient for FakeRpc {
    fn rpc_url(&self) -> &str {
        "fake"
    }

    async fn get_epoch_info(&self, _c: Commitment) -> anyhow::Result<EpochInfo> {
        Ok(self.epoch.clone())
    }

    async fn get_vote_account(
        &self,
        _vote_pubkey: &str,
        _c: Commitment,
    ) -> anyhow::Result<VoteAccount> {
        Ok(self.acct.clone())
    }
}

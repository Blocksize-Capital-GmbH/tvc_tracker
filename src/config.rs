use clap::{Parser, ValueEnum};
use serde::Serialize;

#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct Args {
    /// Vote account pubkey (base58)
    #[arg(long)]
    pub vote_pubkey: String,

    /// HTTP RPC URL (public endpoints are rate-limited)
    #[arg(long, default_value = "https://api.mainnet.solana.com")]
    pub rpc_url: String,

    /// Commitment: finalized is closest to "rooted" accounting
    #[arg(long, default_value = "finalized")]
    pub commitment: Commitment,

    /// Poll interval seconds (poll mode)
    #[arg(long, default_value_t = 60)]
    pub interval_secs: u64,

    /// Directory to write logs to
    #[arg(long, default_value = "logs")]
    pub log_dir: String,

    /// Port to serve metrics on
    #[arg(long, default_value_t = 7999)]
    pub metrics_port: u16,
}

#[derive(Copy, Clone, Debug, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Commitment {
    Processed,
    Confirmed,
    Finalized,
}

impl Args {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.vote_pubkey.trim().is_empty() {
            anyhow::bail!("--vote-pubkey must not be empty");
        }
        Ok(())
    }
}

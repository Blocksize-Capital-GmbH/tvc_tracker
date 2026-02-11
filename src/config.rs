use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct Args {
    /// Vote account pubkey (base58)
    #[arg(long)]
    pub vote_pubkey: String,

    /// RPC URL (WebSocket will be derived automatically: https:// -> wss://)
    #[arg(long, default_value = "https://api.mainnet.solana.com")]
    pub rpc_url: String,

    /// Directory to write logs to
    #[arg(long, default_value = "logs")]
    pub log_dir: String,

    /// Port to serve metrics on
    #[arg(long, default_value_t = 7999)]
    pub metrics_port: u16,
}

impl Args {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.vote_pubkey.trim().is_empty() {
            anyhow::bail!("--vote-pubkey must not be empty");
        }
        Ok(())
    }
}

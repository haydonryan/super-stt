// SPDX-License-Identifier: GPL-3.0-only
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install default crypto provider");

    super_stt::run().await?;
    Ok(())
}

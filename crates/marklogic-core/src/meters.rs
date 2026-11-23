//! Native performance metering documents from MarkLogic's Meters database.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::client::MarkLogicClient;

#[derive(Debug, Deserialize, Serialize)]
pub struct RawSample {
    pub uri: String,
    pub xml: String,
}

/// Read a bounded set of raw XML documents, preserving the source URI and XML.
pub async fn raw_samples(mut client: MarkLogicClient, database: String) -> Result<Vec<RawSample>> {
    client.set_database(database);
    let value = client.configuration_query(include_str!("raw-meters.xqy")).await
        .context("Could not read raw meter documents. The meters database needs a URI lexicon, and the connection needs eval and read privileges")?;
    serde_json::from_value(value).context("Invalid raw meters response")
}

use anyhow::{anyhow, Context, Result};
use aws_config::SdkConfig;

/// Everything we need from an EKS cluster to write a kube config entry.
pub struct ClusterInfo {
    pub endpoint: String,
    pub certificate_authority: String,
    pub private: bool,
}

/// List the names of the EKS clusters reachable with the given config.
pub async fn list_clusters(config: &SdkConfig) -> Result<Vec<String>> {
    let client = aws_sdk_eks::Client::new(config);

    let mut names = Vec::new();
    let mut next_token: Option<String> = None;

    loop {
        let resp = client
            .list_clusters()
            .set_next_token(next_token)
            .send()
            .await
            .context("failed to list EKS clusters")?;

        names.extend(resp.clusters().iter().cloned());

        match resp.next_token() {
            Some(token) => next_token = Some(token.to_string()),
            None => break,
        }
    }

    Ok(names)
}

/// Fetch the endpoint, CA data and private-access flag for a single cluster.
pub async fn describe_cluster(config: &SdkConfig, cluster_name: &str) -> Result<ClusterInfo> {
    let client = aws_sdk_eks::Client::new(config);

    let resp = client
        .describe_cluster()
        .name(cluster_name)
        .send()
        .await
        .with_context(|| format!("failed to describe cluster '{cluster_name}'"))?;

    let cluster = resp
        .cluster()
        .ok_or_else(|| anyhow!("cluster '{cluster_name}' not found"))?;

    let endpoint = cluster
        .endpoint()
        .ok_or_else(|| anyhow!("cluster '{cluster_name}' has no endpoint"))?
        .to_string();

    let certificate_authority = cluster
        .certificate_authority()
        .and_then(|ca| ca.data())
        .ok_or_else(|| anyhow!("cluster '{cluster_name}' has no certificate authority"))?
        .to_string();

    let private = cluster
        .resources_vpc_config()
        .map(|vpc| vpc.endpoint_private_access())
        .unwrap_or(false);

    Ok(ClusterInfo {
        endpoint,
        certificate_authority,
        private,
    })
}

use anyhow::{anyhow, Result};
use aws_config::SdkConfig;

use crate::aws::{bastion as aws_bastion, eks, session};
use crate::commands::{free_port, select_bastion, validate_port};
use crate::kube_config::{self, KubeEntry};
use crate::prompt;

/// Configure `~/.kube/config` for an EKS cluster, prompting interactively for
/// any values that were not supplied on the command line.
#[allow(clippy::too_many_arguments)]
pub async fn update(
    cluster_name: Option<String>,
    profile: Option<String>,
    region: String,
    alias: Option<String>,
    port: Option<u16>,
    bastion: Option<String>,
    no_bastion: bool,
) -> Result<()> {
    let profile = prompt::resolve_profile(profile)?;

    let (cluster_name, info, use_bastion, bastion_name) =
        session::with_sso_retry(&profile, || async {
            let config = session::load_config(&profile, &region).await;

            let cluster_name = match &cluster_name {
                Some(name) => name.clone(),
                None => {
                    let clusters = eks::list_clusters(&config).await?;
                    if clusters.is_empty() {
                        return Err(anyhow!(
                            "no EKS clusters found for profile '{profile}' in {region}"
                        ));
                    }
                    let index = prompt::select("Select a cluster", &clusters)?;
                    clusters[index].clone()
                }
            };

            let info = eks::describe_cluster(&config, &cluster_name).await?;

            // Decide whether (and through which bastion) to route this cluster.
            let (use_bastion, bastion_name) =
                resolve_bastion(&config, &region, bastion.clone(), no_bastion, info.private)
                    .await?;

            Ok((cluster_name, info, use_bastion, bastion_name))
        })
        .await?;

    // Let the user rename the context; pressing Enter keeps the cluster name.
    let alias = match alias {
        Some(alias) => alias,
        None => prompt::text_input_with_default("Context alias", &cluster_name)?,
    };

    let port = match port {
        Some(port) => validate_port(port)?,
        None => free_port()?,
    };

    let outcome = kube_config::add_entry(&KubeEntry {
        alias: alias.clone(),
        cluster_name: cluster_name.clone(),
        region: region.clone(),
        profile: profile.clone(),
        endpoint: info.endpoint,
        certificate_authority: info.certificate_authority,
        use_bastion,
        bastion_name: bastion_name.clone(),
        port,
    })?;

    // The user declined to overwrite an existing entry; nothing more to report.
    if outcome == kube_config::WriteOutcome::Aborted {
        return Ok(());
    }

    let visibility = if info.private { "private" } else { "public" };
    println!(
        "Configured {visibility} cluster '{cluster_name}' in {region} (profile '{profile}') as context '{alias}'."
    );
    if use_bastion {
        match &bastion_name {
            Some(name) => println!(
                "It will bridge through bastion '{name}' on port {port} automatically when used."
            ),
            None => println!(
                "A SOCKS bridge on port {port} will start automatically when you use this context."
            ),
        }
    }

    Ok(())
}

/// Determine whether to route through a bastion and, if so, which one.
///
/// - `--no-bastion` forces it off.
/// - `--bastion <name>` pins a specific bastion.
/// - Otherwise the user is asked (defaulting to the cluster's private-access
///   flag), then picks a bastion when more than one is discovered.
async fn resolve_bastion(
    config: &SdkConfig,
    region: &str,
    bastion_arg: Option<String>,
    no_bastion: bool,
    private: bool,
) -> Result<(bool, Option<String>)> {
    if no_bastion {
        return Ok((false, None));
    }

    if let Some(name) = bastion_arg {
        // Validate now, while a human is watching and errors are visible, rather
        // than failing later inside a silent kubectl exec call.
        let bastions = aws_bastion::find_bastions(config)
            .await
            .map_err(|e| anyhow!("could not discover bastions in {region}: {e}"))?;
        if !bastions.iter().any(|b| b.name == name) {
            let available: Vec<String> = bastions.iter().map(|b| b.name.clone()).collect();
            return Err(anyhow!(
                "bastion '{name}' not found in {region}. Available: {}",
                available.join(", ")
            ));
        }
        return Ok((true, Some(name)));
    }

    if !prompt::confirm("Route this cluster through a bastion?", private)? {
        return Ok((false, None));
    }

    let bastions = aws_bastion::find_bastions(config)
        .await
        .map_err(|e| anyhow!("could not discover bastions in {region}: {e}"))?;
    let bastion = select_bastion(bastions)?;

    Ok((true, Some(bastion.name)))
}

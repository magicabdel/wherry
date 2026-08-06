use anyhow::{anyhow, Context, Result};
use aws_config::SdkConfig;

/// Tag conventions used to discover bastion resources.
const ASG_NAME_TAG: &str = "ASG-bastion-lt";
const LB_USAGE_TAG_KEY: &str = "usage";
const LB_USAGE_TAG_VALUE: &str = "bastion";
const BASTION_OS_USER: &str = "ec2-user";

/// A bastion the user can bridge through: an SSH endpoint (the load balancer
/// DNS name) backed by an EC2 instance we can push a key to.
#[derive(Clone)]
pub struct Bastion {
    pub name: String,
    pub dns: String,
    pub instance_id: String,
    pub availability_zone: String,
}

impl Bastion {
    pub fn os_user(&self) -> &'static str {
        BASTION_OS_USER
    }
}

/// Discover bastions in the account/region.
///
/// A bastion is a load balancer tagged `usage=bastion` fronting the instances
/// of the `ASG-bastion-lt` auto scaling group. Each discovered load balancer is
/// paired with a running bastion instance so its key can be pushed via EC2
/// Instance Connect.
pub async fn find_bastions(config: &SdkConfig) -> Result<Vec<Bastion>> {
    let instances = bastion_instances(config).await?;
    if instances.is_empty() {
        return Err(anyhow!(
            "no bastion instance found in auto scaling group tagged Name={ASG_NAME_TAG}"
        ));
    }

    let load_balancers = bastion_load_balancers(config).await?;
    if load_balancers.is_empty() {
        return Err(anyhow!(
            "no bastion load balancer found (tagged {LB_USAGE_TAG_KEY}={LB_USAGE_TAG_VALUE})"
        ));
    }

    // The common topology is a single ASG instance behind each bastion load
    // balancer, so every load balancer is paired with the first live instance.
    let (instance_id, availability_zone) = instances[0].clone();

    let bastions = load_balancers
        .into_iter()
        .map(|(name, dns)| Bastion {
            name,
            dns,
            instance_id: instance_id.clone(),
            availability_zone: availability_zone.clone(),
        })
        .collect();

    Ok(bastions)
}

/// Push an SSH public key to the bastion instance via EC2 Instance Connect.
///
/// Returns `true` when AWS accepted the key.
pub async fn send_public_key(
    config: &SdkConfig,
    bastion: &Bastion,
    public_key: &str,
) -> Result<bool> {
    let client = aws_sdk_ec2instanceconnect::Client::new(config);

    let resp = client
        .send_ssh_public_key()
        .instance_id(&bastion.instance_id)
        .instance_os_user(BASTION_OS_USER)
        .availability_zone(&bastion.availability_zone)
        .ssh_public_key(public_key)
        .send()
        .await
        .context("failed to send SSH public key via EC2 Instance Connect")?;

    Ok(resp.success())
}

/// Return `(instance_id, availability_zone)` for each instance in the bastion ASG.
async fn bastion_instances(config: &SdkConfig) -> Result<Vec<(String, String)>> {
    let client = aws_sdk_autoscaling::Client::new(config);

    let resp = client
        .describe_auto_scaling_groups()
        .filters(
            aws_sdk_autoscaling::types::Filter::builder()
                .name("tag:Name")
                .values(ASG_NAME_TAG)
                .build(),
        )
        .send()
        .await
        .context("failed to describe bastion auto scaling group")?;

    let instances = resp
        .auto_scaling_groups()
        .iter()
        .flat_map(|group| group.instances())
        .filter_map(|instance| {
            let id = instance.instance_id()?;
            let az = instance.availability_zone()?;
            Some((id.to_string(), az.to_string()))
        })
        .collect();

    Ok(instances)
}

/// Return `(name, dns_name)` for each load balancer tagged as a bastion.
async fn bastion_load_balancers(config: &SdkConfig) -> Result<Vec<(String, String)>> {
    let tagging = aws_sdk_resourcegroupstagging::Client::new(config);

    let resources = tagging
        .get_resources()
        .tag_filters(
            aws_sdk_resourcegroupstagging::types::TagFilter::builder()
                .key(LB_USAGE_TAG_KEY)
                .values(LB_USAGE_TAG_VALUE)
                .build(),
        )
        .resource_type_filters("elasticloadbalancing:loadbalancer")
        .send()
        .await
        .context("failed to look up bastion load balancers by tag")?;

    let arns: Vec<String> = resources
        .resource_tag_mapping_list()
        .iter()
        .filter_map(|mapping| mapping.resource_arn().map(str::to_string))
        .collect();

    if arns.is_empty() {
        return Ok(Vec::new());
    }

    let elb = aws_sdk_elasticloadbalancingv2::Client::new(config);

    let resp = elb
        .describe_load_balancers()
        .set_load_balancer_arns(Some(arns))
        .send()
        .await
        .context("failed to describe bastion load balancers")?;

    let load_balancers = resp
        .load_balancers()
        .iter()
        .filter_map(|lb| {
            let dns = lb.dns_name()?.to_string();
            let name = lb
                .load_balancer_name()
                .map(str::to_string)
                .unwrap_or_else(|| dns.clone());
            Some((name, dns))
        })
        .collect();

    Ok(load_balancers)
}

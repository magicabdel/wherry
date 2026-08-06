mod aws;
mod bridge;
mod commands;
mod kube_config;
mod prompt;
mod ssh;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "wherry",
    version,
    about = "Configure kubectl access to (private) EKS clusters via an automatic bastion bridge"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Configure ~/.kube/config for a cluster (interactive when arguments are omitted)
    UpdateKubeconfig(UpdateKubeConfigArgs),
    /// Manage the SOCKS bridge to a bastion
    Bridge {
        #[command(subcommand)]
        cmd: BridgeCmd,
    },
    /// Manage AWS SSO tokens
    Sso {
        #[command(subcommand)]
        cmd: SsoCmd,
    },
}

#[derive(Args)]
struct UpdateKubeConfigArgs {
    /// Name of the cluster to set up. Prompted interactively if omitted.
    cluster_name: Option<String>,
    /// AWS profile to use. Prompted interactively if omitted.
    #[arg(long)]
    profile: Option<String>,
    /// AWS region the cluster lives in.
    #[arg(long, default_value = "eu-west-1")]
    region: String,
    /// Alias for the context name. Prompted interactively if omitted
    /// (defaults to the cluster name).
    #[arg(long)]
    alias: Option<String>,
    /// Local SOCKS port for the bridge. Defaults to a random free port.
    #[arg(long)]
    port: Option<u16>,
    /// Pin a specific bastion by name (implies routing through a bastion).
    #[arg(long)]
    bastion: Option<String>,
    /// Do not route this cluster through a bastion (skips the prompt).
    #[arg(long, conflicts_with = "bastion")]
    no_bastion: bool,
}

#[derive(Subcommand)]
enum BridgeCmd {
    /// Start a SOCKS bridge to the bastion (interactive when arguments are omitted)
    Start(StartArgs),
    /// Stop a running bridge daemon
    Stop(StopArgs),
}

#[derive(Args)]
struct StartArgs {
    /// AWS profile to use. Prompted interactively if omitted.
    #[arg(long)]
    profile: Option<String>,
    /// AWS region the bastion lives in.
    #[arg(long, default_value = "eu-west-1")]
    region: String,
    /// Local SOCKS port for the bridge.
    #[arg(long, default_value_t = commands::DEFAULT_BRIDGE_PORT)]
    port: u16,
    /// Pin a specific bastion by name. Prompted when several are found.
    #[arg(long)]
    bastion: Option<String>,
}

#[derive(Args)]
struct StopArgs {
    /// Port of the bridge to stop. Defaults to the standard bridge port.
    #[arg(long)]
    port: Option<u16>,
    /// Stop every running wherry bridge.
    #[arg(long, conflicts_with = "port")]
    all: bool,
}

#[derive(Subcommand)]
enum SsoCmd {
    /// Refresh (or establish) an AWS SSO token, prompting when no target is given
    Login(SsoTargetArgs),
    /// Show the freshness of configured AWS SSO tokens
    Status(SsoTargetArgs),
}

#[derive(Args)]
struct SsoTargetArgs {
    /// AWS profile whose SSO token should be checked/refreshed
    #[arg(long, conflicts_with = "session")]
    profile: Option<String>,
    /// Name of an `sso-session` (from ~/.aws/config) to check/refresh directly
    #[arg(long, conflicts_with = "profile")]
    session: Option<String>,
    /// Only print something when a login was actually needed (used by the
    /// generated kube config exec plugin)
    #[arg(long)]
    quiet: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::UpdateKubeconfig(args) => {
            // AWS calls are async; run them on a temporary runtime.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(commands::kubeconfig::update(
                args.cluster_name,
                args.profile,
                args.region,
                args.alias,
                args.port,
                args.bastion,
                args.no_bastion,
            ))
        }
        Command::Bridge { cmd } => match cmd {
            // `bridge start` manages its own runtimes because it daemonizes.
            BridgeCmd::Start(args) => {
                commands::bridge::start(args.profile, args.region, args.port, args.bastion)
            }
            BridgeCmd::Stop(args) => commands::bridge::stop(args.port, args.all),
        },
        Command::Sso { cmd } => match cmd {
            SsoCmd::Login(args) => commands::sso::login(args.profile, args.session, args.quiet),
            SsoCmd::Status(args) => commands::sso::status(args.profile, args.session),
        },
    }
}

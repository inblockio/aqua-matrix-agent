use anyhow::{anyhow, Result};
use aqua_matrix_agent::{did_from_key_file, AgentClient, AgentConfig};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "aqua-matrix-agent",
    about = "Matrix agent: authenticate via siwx-oidc, send and read E2EE messages"
)]
struct Args {
    #[arg(long, env = "AGENT_KEY_FILE", default_value = "agent.pem")]
    key_file: PathBuf,

    #[arg(long, env = "SIWX_URL", default_value = "https://siwx-oidc.inblock.io")]
    siwx_url: String,

    #[arg(long, env = "MATRIX_URL", default_value = "https://matrix.inblock.io")]
    matrix_url: String,

    #[arg(long, env = "OIDC_CLIENT_ID", required_unless_present = "print_did")]
    client_id: Option<String>,

    #[arg(long, env = "OIDC_REDIRECT_URI", required_unless_present = "print_did")]
    redirect_uri: Option<String>,

    #[arg(
        long,
        default_value = "@did-pkh-eip155-1-0x0000000000000000000000000000000000000000:matrix.inblock.io"
    )]
    target: String,

    #[arg(long, env = "AGENT_STORE_DIR")]
    store_dir: Option<PathBuf>,

    #[arg(long, help = "Message to send (omit to skip sending)")]
    message: Option<String>,

    #[arg(long, help = "Read recent messages from the DM room")]
    read: bool,

    #[arg(long, default_value = "20")]
    read_limit: u32,

    #[arg(long, help = "Print agent DID and exit")]
    print_did: bool,
}

fn default_store_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".aqua-matrix-agent")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,aqua_matrix_agent=info".into()),
        )
        .init();

    let args = Args::parse();

    if args.print_did {
        println!("{}", did_from_key_file(&args.key_file)?);
        return Ok(());
    }

    let config = AgentConfig {
        key_file: args.key_file,
        siwx_url: args.siwx_url,
        matrix_url: args.matrix_url,
        client_id: args
            .client_id
            .ok_or_else(|| anyhow!("--client-id required"))?,
        redirect_uri: args
            .redirect_uri
            .ok_or_else(|| anyhow!("--redirect-uri required"))?,
        store_dir: args.store_dir.unwrap_or_else(default_store_dir),
    };

    let agent = AgentClient::connect(config).await?;

    let joined = agent.join_invited_rooms().await?;
    if !joined.is_empty() {
        agent.sync_once().await?;
    }

    if let Some(ref msg) = args.message {
        let event_id = agent.send_dm(&args.target, msg).await?;
        println!("sent to {}: {} (event: {event_id})", args.target, msg);
    }

    if args.read {
        if args.message.is_some() {
            agent.sync_once().await?;
        }
        match agent.dm_room_id(&args.target).await? {
            Some(room_id) => {
                let messages = agent.messages(&room_id, args.read_limit).await?;
                if messages.is_empty() {
                    println!("no messages found");
                } else {
                    for msg in &messages {
                        println!("[{}] {}: {}", msg.timestamp_ms, msg.sender, msg.body);
                    }
                }
            }
            None => println!("no DM room found with {}", args.target),
        }
    }

    if args.message.is_none() && !args.read {
        println!("connected as {} ({})", agent.user_id(), agent.did());
        println!("use --message to send or --read to read messages");
    }

    Ok(())
}

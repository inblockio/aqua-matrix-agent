//! The smallest possible aqua-matrix agent: echo every DM back to the sender.
//!
//! This is the whole contract — implement `MessageHandler`, build an
//! `AgentConfig`, call `run_daemon`. No matrix-sdk, no auth code, no sync loop.
//! Everything below the `main` is ~15 lines of actual agent logic.
//!
//! Run with:  cargo run -p aqua-matrix-relay --example echo_agent
//! (generates `echo.pem` on first run; talks to the default homeserver.)

use std::path::PathBuf;

use aqua_matrix_relay::{async_trait, run_daemon, AgentClient, AgentConfig, MessageHandler};

struct EchoHandler;

#[async_trait]
impl MessageHandler for EchoHandler {
    fn role(&self) -> &str {
        "echo"
    }

    fn hello(&self, agent: &AgentClient) -> Option<String> {
        Some(format!("[echo] online as {}. I echo whatever you DM me.", agent.user_id()))
    }

    async fn handle_message(&self, agent: &AgentClient, target: &str, body: &str) {
        if let Err(e) = agent.send_dm(target, &format!("echo: {body}")).await {
            eprintln!("echo reply failed: {e:#}");
        }
    }
}

#[tokio::main]
async fn main() {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let config = AgentConfig {
        key_file: PathBuf::from("echo.pem"),
        siwx_url: "https://siwx-oidc.inblock.io".into(),
        matrix_url: "https://matrix.inblock.io".into(),
        client_id: None,
        redirect_uri: None,
        store_dir: PathBuf::from(home).join(".aqua-matrix-echo"),
    };

    // Replace this target with whoever should be allowed to talk to the agent.
    let target = "@did-pkh-eip155-1-0x0000000000000000000000000000000000000000:matrix.inblock.io";
    run_daemon(config, target, EchoHandler).await;
}

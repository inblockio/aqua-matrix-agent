#![cfg(feature = "e2e")]

use aqua_matrix_agent::{AgentClient, AgentConfig};
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn agent_config(key_file: &str, store_dir: &str) -> AgentConfig {
    AgentConfig {
        key_file: repo_root().join(key_file),
        siwx_url: "https://siwx-oidc.inblock.io".into(),
        matrix_url: "https://matrix.inblock.io".into(),
        client_id: None,
        redirect_uri: None,
        store_dir: PathBuf::from(store_dir),
    }
}

async fn sync_n(agent: &AgentClient, n: usize) {
    for _ in 0..n {
        agent.sync_once().await.expect("sync failed");
    }
}

fn clean_store(path: &str) {
    let _ = std::fs::remove_dir_all(path);
}

#[tokio::test]
async fn e2ee_bidirectional_messaging() {
    tracing_subscriber::fmt()
        .with_env_filter("warn,aqua_matrix_agent=info")
        .try_init()
        .ok();

    // Wipe stores to avoid stale crypto keys from prior runs or CLI usage.
    // The device_id is deterministic per DID, so any other crypto store that
    // used the same key file will have uploaded conflicting identity keys.
    clean_store("/tmp/aqua-e2e-agent-a");
    clean_store("/tmp/aqua-e2e-agent-b");

    // Connect both agents via CAIP-122 auth
    let agent_a = AgentClient::connect(agent_config("agent.pem", "/tmp/aqua-e2e-agent-a"))
        .await
        .expect("Agent A failed to connect");
    println!("Agent A connected: {} ({})", agent_a.user_id(), agent_a.did());

    let agent_b = AgentClient::connect(agent_config("agent-b.pem", "/tmp/aqua-e2e-agent-b"))
        .await
        .expect("Agent B failed to connect");
    println!("Agent B connected: {} ({})", agent_b.user_id(), agent_b.did());

    assert_ne!(agent_a.user_id(), agent_b.user_id(), "agents must have different identities");

    // Phase 1: Establish DM room and get both agents into it.
    // The setup message creates the room and invites Agent B.
    agent_a
        .send_dm(agent_b.user_id(), "e2e-room-setup")
        .await
        .expect("Agent A failed to create DM room");
    println!("DM room created by Agent A");

    // Agent B joins the room
    sync_n(&agent_b, 2).await;
    agent_b.join_invited_rooms().await.expect("Agent B join failed");
    sync_n(&agent_b, 2).await;
    println!("Agent B joined the room");

    // Agent A learns about Agent B's device keys
    sync_n(&agent_a, 2).await;

    // Phase 2: Agent B sends first (B created its outbound session AFTER joining,
    // so B's session key is shared with A from the start).
    let tag = uuid::Uuid::new_v4().to_string();

    let msg_b_to_a = format!("e2e-test-b-to-a-{tag}");
    let event_id = agent_b
        .send_dm(agent_a.user_id(), &msg_b_to_a)
        .await
        .expect("Agent B failed to send");
    println!("Agent B sent: {msg_b_to_a} (event: {event_id})");

    // Agent A syncs to receive the message and key-sharing events
    sync_n(&agent_a, 2).await;

    let room_id = agent_a
        .dm_room_id(agent_b.user_id())
        .await
        .expect("failed to get DM room")
        .expect("no DM room found between agents");

    let messages = agent_a
        .messages(&room_id, 10)
        .await
        .expect("Agent A failed to read messages");

    let found = messages.iter().find(|m| m.body == msg_b_to_a);
    assert!(
        found.is_some(),
        "Agent A did not find message from Agent B: {msg_b_to_a}\nMessages: {:?}",
        messages.iter().map(|m| &m.body).collect::<Vec<_>>()
    );
    assert_ne!(
        found.unwrap().body, "[unable to decrypt]",
        "message from B was not decryptable by A (E2EE key exchange failed)"
    );
    println!("Agent A received and decrypted: {msg_b_to_a}");

    // Phase 3: Agent A replies (bidirectional test)
    let msg_a_to_b = format!("e2e-test-a-to-b-{tag}");
    let event_id = agent_a
        .send_dm(agent_b.user_id(), &msg_a_to_b)
        .await
        .expect("Agent A failed to send reply");
    println!("Agent A sent: {msg_a_to_b} (event: {event_id})");

    // Agent B syncs to receive
    sync_n(&agent_b, 2).await;

    let room_id_b = agent_b
        .dm_room_id(agent_a.user_id())
        .await
        .expect("failed to get DM room")
        .expect("no DM room found between agents (reverse direction)");

    let messages = agent_b
        .messages(&room_id_b, 10)
        .await
        .expect("Agent B failed to read messages");

    let found = messages.iter().find(|m| m.body == msg_a_to_b);
    assert!(
        found.is_some(),
        "Agent B did not find reply from Agent A: {msg_a_to_b}\nMessages: {:?}",
        messages.iter().map(|m| &m.body).collect::<Vec<_>>()
    );
    assert_ne!(
        found.unwrap().body, "[unable to decrypt]",
        "reply from A was not decryptable by B (E2EE key exchange failed)"
    );
    println!("Agent B received and decrypted: {msg_a_to_b}");

    println!("\nE2EE bidirectional test PASSED");
    println!("  Agent A: {} ({})", agent_a.user_id(), agent_a.did());
    println!("  Agent B: {} ({})", agent_b.user_id(), agent_b.did());
    println!("  Messages verified decryptable in both directions");
}

use anyhow::{Context, Result};
use config::Config;
use matrix_sdk::{
    Client, SessionMeta,
    authentication::{SessionTokens, matrix::MatrixSession},
    config::SyncSettings,
    room::Room,
    ruma::events::room::{
        member::{MembershipState, StrippedRoomMemberEvent},
        message::OriginalSyncRoomMessageEvent,
        redaction::SyncRoomRedactionEvent,
    },
    store::RoomLoadSettings,
};
use mime_guess::Mime;
use reqwest::Proxy;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

mod activitypub;
mod command;
mod config;
mod extract;
mod handler;
mod media;
mod metadata;
mod processing;
mod tracker;

#[derive(Serialize, Deserialize)]
struct SavedSession {
    user_id: String,
    device_id: String,
    access_token: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load config from CLI args/files
    let config = Config::load().await?;

    // Initialize Client
    // Ensure store directories exist
    std::fs::create_dir_all(&config.state_store_path)?;

    let client = Client::builder()
        .homeserver_url(&config.homeserver_url)
        .sqlite_store(&config.state_store_path, None)
        .build()
        .await
        .context("Failed to build client")?;

    let session_file = config.state_store_path.join("session.json");

    // Check for existing session in store or file
    if client.matrix_auth().session().is_some() {
        // TODO: does this ever happen? Doesn't seem to.
        info!("Restored session from state store.");
    } else if session_file.exists() {
        // Restore from file
        let file = File::open(&session_file)?;
        let reader = BufReader::new(file);
        let saved_session: SavedSession = serde_json::from_reader(reader)?;

        let user_id = matrix_sdk::ruma::UserId::parse(&saved_session.user_id)?;

        let session = MatrixSession {
            meta: SessionMeta {
                user_id,
                device_id: saved_session.device_id.into(),
            },
            tokens: SessionTokens {
                access_token: saved_session.access_token,
                refresh_token: None,
            },
        };

        client
            .matrix_auth()
            .restore_session(session, RoomLoadSettings::default())
            .await
            .context("Failed to restore session")?;
        info!("Restored session from session.json.");
    } else if let Some(token) = &config.access_token {
        // Restore session from config
        let user_id = matrix_sdk::ruma::UserId::parse(&config.username)
            .context("Failed to parse username as UserId for session restoration")?;

        let session = MatrixSession {
            meta: SessionMeta {
                user_id,
                device_id: "MATRIX_EMBED_BOT".into(),
            },
            tokens: SessionTokens {
                access_token: token.clone(),
                refresh_token: None,
            },
        };

        client
            .matrix_auth()
            .restore_session(session, RoomLoadSettings::default())
            .await
            .context("Failed to restore session from access token")?;
        info!("Restored session from access token.");
    } else if !config.username.is_empty() {
        let _response = client
            .matrix_auth()
            .login_username(
                &config.username,
                config.password.as_deref().unwrap_or_default(),
            )
            .send()
            .await
            .context("Failed to login")?;
        info!("Logged in via username/password.");

        // Save session
        if let Some(session) = client.matrix_auth().session() {
            let saved_session = SavedSession {
                user_id: session.meta.user_id.to_string(),
                device_id: session.meta.device_id.to_string(),
                access_token: session.tokens.access_token,
            };
            let file = File::create(&session_file)?;
            serde_json::to_writer(file, &saved_session)?;
            info!("Saved session to session.json");
        }
    } else {
        warn!("No credentials provided.");
    }

    client
        .encryption()
        .wait_for_e2ee_initialization_tasks()
        .await;

    if config.reset_identity {
        client.encryption().recovery().reset_identity().await?;
    }

    let mut http_builder = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)");
    if let Some(proxy) = config.proxy.clone() {
        http_builder = http_builder.proxy(Proxy::all(proxy)?);
    }
    let http_client = http_builder.build()?;
    let config = Arc::new(config);

    // Event tracker for replacement / redaction handling
    let tracker = Arc::new(tracker::EventTracker::new());
    tracker.spawn_cleanup_task();

    // ActivityPub detector for Fediverse support
    let ap_detector = Arc::new(activitypub::ActivityPubDetector::new());

    // Message event handler
    client.add_event_handler({
        let config = config.clone();
        let http_client = http_client.clone();
        let client = client.clone();
        let tracker = tracker.clone();
        let ap_detector = ap_detector.clone();

        move |event: OriginalSyncRoomMessageEvent, room: Room| {
            let config = config.clone();
            let http_client = http_client.clone();
            let client = client.clone();
            let tracker = tracker.clone();
            let ap_detector = ap_detector.clone();
            debug!("Event: {:?}", event);
            async move {
                // Ignore own messages
                if event.sender == room.own_user_id() {
                    return;
                }

                if let Err(e) = handler::handle_message(
                    event,
                    room,
                    config,
                    http_client,
                    client,
                    tracker,
                    ap_detector,
                )
                .await
                {
                    error!("Error handling message: {:?}", e);
                }
            }
        }
    });

    // Redaction event handler
    client.add_event_handler({
        let tracker = tracker.clone();

        move |event: SyncRoomRedactionEvent, room: Room| {
            let tracker = tracker.clone();
            async move {
                if let Err(e) = handler::handle_redaction(event, room, tracker).await {
                    error!("Error handling redaction: {:?}", e);
                }
            }
        }
    });

    // Invite event handler
    client.add_event_handler({
        let config = config.clone();
        move |event: StrippedRoomMemberEvent, room: Room| {
            let config = config.clone();
            async move {
                if event.content.membership != MembershipState::Invite {
                    return;
                }

                info!("Received invite from {}", event.sender);

                if config.trusted_users.contains(&event.sender.to_string()) {
                    info!("Accepting invite from trusted user {}", event.sender);
                    if let Err(e) = room.join().await {
                        error!("Failed to join room: {:?}", e);
                    }
                } else {
                    warn!("Ignoring invite from untrusted user {}", event.sender);
                }
            }
        }
    });

    // By this point, we should be authenticated. Set up any initial account configuration if needed.
    info!("Configuring any relevant account settings if needed...");

    if let Some(avatar_data) = config.avatar_data.clone()
        && client
            .account()
            .get_avatar_url()
            .await
            .ok()
            .flatten()
            .is_none()
    {
        info!("No avatar set. Setting avatar.");
        let kind = infer::get(&avatar_data)
            .map(|t| t.mime_type())
            .unwrap_or("image/png")
            .parse::<Mime>()?;
        client.account().upload_avatar(&kind, avatar_data).await?;
        info!("Avatar should be good to go now.")
    }

    info!("Bot started, syncing...");
    client.sync(SyncSettings::default()).await?;

    Ok(())
}

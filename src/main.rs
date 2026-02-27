use anyhow::{Context, Result};
use config::Config;
use matrix_sdk::{
    Client, SessionMeta,
    authentication::{SessionTokens, matrix::MatrixSession},
    config::SyncSettings,
    room::{Room, reply::Reply},
    ruma::events::room::{
        member::{MembershipState, StrippedRoomMemberEvent},
        message::{
            AddMentions, ForwardThread, MessageType, OriginalSyncRoomMessageEvent,
            RoomMessageEventContent, TextMessageEventContent,
        },
    },
    store::RoomLoadSettings,
};
use metadata::Metadata;
use mime_guess::Mime;
use reqwest::Proxy;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::future::Future;
use std::io::BufReader;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};
use url::Url;

use crate::processing::{MessageParams, process_metadata, process_response};

mod config;
mod media;
mod metadata;
mod processing;

#[derive(Serialize, Deserialize)]
struct SavedSession {
    user_id: String,
    device_id: String,
    access_token: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

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

    // Event Handler
    client.add_event_handler({
        let config = config.clone();
        let http_client = http_client.clone();

        move |event: OriginalSyncRoomMessageEvent, room: Room| {
            let config = config.clone();
            let http_client = http_client.clone();
            debug!("Event: {:?}", event);
            async move {
                // Ignore own messages
                if event.sender == room.own_user_id() {
                    return;
                }

                if let Err(e) = handle_message(event, room, config, http_client).await {
                    error!("Error handling message: {:?}", e);
                }
            }
        }
    });

    // Handle invites
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

async fn handle_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
    config: Arc<Config>,
    http_client: reqwest::Client,
) -> Result<()> {
    let msgtype = match event.content.msgtype.clone() {
        MessageType::Text(t) => t,
        _ => return Ok(()),
    };

    let body = msgtype.body;
    for word in body.split_whitespace() {
        if (word.starts_with("http://") || word.starts_with("https://"))
            && let Ok(url) = Url::parse(word)
        {
            // Apply URL rewrites
            let url = config.rewrite_url(&url);
            debug!("Found URL: {}", url);
            if let Err(e) = process_url(&http_client, &room, &config, &url, event.clone()).await {
                warn!("Failed to process URL {}: {:?}", url, e);
            }
            // Only process the first URL found (for now?)
            break;
        }
    }

    Ok(())
}

async fn process_url(
    http_client: &reqwest::Client,
    room: &Room,
    config: &Config,
    url: &Url,
    reply: OriginalSyncRoomMessageEvent,
) -> Result<()> {
    match Metadata::fetch_from_url(http_client, url).await {
        Ok(meta) => {
            debug!("Metadata: {:?}", meta);
            if meta.is_empty() {
                return Ok(());
            }
            let params = process_metadata(meta, config);
            post_message(http_client, room, config, params, reply).await?;
        }
        Err(e) => {
            warn!("Failed to fetch metadata for {}: {:?}", url, e);
        }
    }
    Ok(())
}

async fn with_typing<F, T>(room: &Room, fut: F) -> T
where
    F: Future<Output = T>,
{
    let typing_room = room.clone();
    let typing_task = tokio::spawn(async move {
        loop {
            let _ = typing_room.typing_notice(true).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    let result = fut.await;

    typing_task.abort();
    let _ = room.typing_notice(false).await;

    result
}

async fn post_message(
    http_client: &reqwest::Client,
    room: &Room,
    config: &Config,
    params: MessageParams,
    reply: OriginalSyncRoomMessageEvent,
) -> Result<()> {
    let has_text = !params.body.is_empty() || !params.html_body.is_empty();

    let caption = if has_text {
        Some(TextMessageEventContent::html(
            params.body.clone(),
            params.html_body.clone(),
        ))
    } else {
        None
    };

    if let Some(media_url) = params.media_url {
        info!("Downloading media from {}", media_url);

        let result = with_typing(
            room,
            download_and_upload(
                http_client,
                room,
                &media_url,
                config,
                caption,
                Reply {
                    event_id: reply.event_id.clone(),
                    enforce_thread: matrix_sdk::room::reply::EnforceThread::MaybeThreaded,
                },
            ),
        )
        .await;

        if let Err(e) = result {
            error!("Failed to upload media: {:?}", e);
            // Fallback: Reply with text embed if failed
            if has_text {
                room.send(
                    RoomMessageEventContent::text_html(params.body, params.html_body)
                        .make_reply_to(&reply, ForwardThread::Yes, AddMentions::No),
                )
                .await?;
            }
        }
    } else if has_text {
        // Just text embed if no media
        room.send(
            RoomMessageEventContent::text_html(params.body, params.html_body).make_reply_to(
                &reply,
                ForwardThread::Yes,
                AddMentions::No,
            ),
        )
        .await?;
    }
    Ok(())
}

pub async fn download_and_upload(
    client: &reqwest::Client,
    room: &Room,
    url: &Url,
    config: &Config,
    text: Option<TextMessageEventContent>,
    reply: Reply,
) -> Result<()> {
    let response = client
        .get(url.clone())
        .timeout(config.download_timeout)
        .send()
        .await
        .context("Failed to start download")?;

    let attachment = process_response(response, config, text).await?;

    // Generic file upload handles images/videos based on MIME type usually
    room.send_attachment(
        &attachment.filename,
        &attachment.mime_type,
        attachment.data,
        attachment.attachment_config.reply(Some(reply)),
    )
    .await?;

    Ok(())
}

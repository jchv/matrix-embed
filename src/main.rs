use anyhow::{Context, Result, bail};
use config::Config;
use matrix_sdk::{
    Client, SessionMeta,
    authentication::{SessionTokens, matrix::MatrixSession},
    config::SyncSettings,
    encryption::VerificationState,
    room::Room,
    ruma::{
        OwnedDeviceId,
        api::client::uiaa,
        events::room::{
            member::{MembershipState, StrippedRoomMemberEvent, SyncRoomMemberEvent},
            message::OriginalSyncRoomMessageEvent,
            redaction::SyncRoomRedactionEvent,
        },
    },
    store::RoomLoadSettings,
};
use mime_guess::Mime;
use reqwest::Proxy;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

mod activitypub;
mod cas;
mod command;
mod config;
mod db;
mod extract;
mod handler;
mod key_sharing;
mod media;
mod metadata;
mod processing;
mod tracker;

/// Persisted session data.
///
/// The `homeserver` and `refresh_token` fields are temporarily optional.
#[derive(Serialize, Deserialize)]
struct SavedSession {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    homeserver: Option<String>,
    user_id: String,
    device_id: String,
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load config from CLI args / files.
    let config = Config::load().await?;
    let session_file = config.state_store_path.join("session.json");

    // Authenticate
    let client = restore_or_login(&config, &session_file).await?;

    client
        .encryption()
        .wait_for_e2ee_initialization_tasks()
        .await;

    if config.reset_identity {
        info!("--reset-identity flag is set; resetting cryptographic identity...");
        client.encryption().recovery().reset_identity().await?;
    }

    ensure_verified(&client, &config).await;
    spawn_session_change_listener(&client, session_file);

    let mut http_builder = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)");
    if let Some(proxy) = config.proxy.clone() {
        http_builder = http_builder.proxy(Proxy::all(proxy)?);
    }
    let http_client = http_builder.build()?;
    // Open (or create) the persistent database.
    let database = db::Database::open(&config.database_path).await?;
    let database = Arc::new(database);

    // Open (or create) the content-addressable media store.
    let media_store = cas::MediaStore::open(&config.media_store_path).await?;
    let media_store = Arc::new(media_store);

    let config = Arc::new(config);

    let tracker = Arc::new(tracker::EventTracker::new());
    tracker.spawn_cleanup_task();

    let ap_detector = Arc::new(activitypub::ActivityPubDetector::new());

    // Message handler
    client.add_event_handler({
        let config = config.clone();
        let http_client = http_client.clone();
        let client = client.clone();
        let tracker = tracker.clone();
        let ap_detector = ap_detector.clone();
        let database = database.clone();
        let media_store = media_store.clone();

        move |event: OriginalSyncRoomMessageEvent, room: Room| {
            let config = config.clone();
            let http_client = http_client.clone();
            let client = client.clone();
            let tracker = tracker.clone();
            let ap_detector = ap_detector.clone();
            let database = database.clone();
            let media_store = media_store.clone();
            debug!("Event: {:?}", event);
            async move {
                // Ignore own messages.
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
                    database,
                    media_store,
                )
                .await
                {
                    error!("Error handling message: {:?}", e);
                }
            }
        }
    });

    // Redaction handler
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

    // Membership handler — detect new joins for room-key sharing.
    client.add_event_handler({
        let database = database.clone();
        let client_for_keys = client.clone();
        move |event: SyncRoomMemberEvent, room: Room| {
            let database = database.clone();
            let client_for_keys = client_for_keys.clone();
            async move {
                let SyncRoomMemberEvent::Original(event) = event else {
                    return;
                };

                // Only care about users who just joined.
                if event.content.membership != MembershipState::Join {
                    return;
                }

                // Ignore our own joins.
                if event.state_key == room.own_user_id().as_str() {
                    return;
                }

                // Check whether this room has key sharing enabled.
                let room_id_str = room.room_id().to_string();
                match database.is_key_sharing_enabled(&room_id_str).await {
                    Ok(true) => {
                        let user_id =
                            match matrix_sdk::ruma::UserId::parse(event.state_key.as_str()) {
                                Ok(id) => id,
                                Err(e) => {
                                    warn!(
                                        "Invalid user ID in membership event: {}: {}",
                                        event.state_key, e
                                    );
                                    return;
                                }
                            };
                        let room_id = room.room_id().to_owned();
                        info!(
                            "User {} joined key-sharing room {}; \
                             spawning room-key distribution task",
                            user_id, room_id
                        );
                        tokio::spawn(async move {
                            if let Err(e) = key_sharing::share_room_history(
                                &client_for_keys,
                                &room_id,
                                &user_id,
                            )
                            .await
                            {
                                error!(
                                    "Failed to share room history with {} in {}: {:?}",
                                    user_id, room_id, e
                                );
                            }
                        });
                    }
                    Ok(false) => {}
                    Err(e) => {
                        error!(
                            "Failed to check key-sharing status for room {}: {:?}",
                            room_id_str, e
                        );
                    }
                }
            }
        }
    });

    // Invite handler
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

    // Account setup (avatar)
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
        info!("Avatar should be good to go now.");
    }

    // Sync loop
    info!("Bot started, syncing...");
    client.sync(SyncSettings::default()).await?;

    Ok(())
}

/// Top-level authentication flow.
///
/// 1. If a `session.json` exists, try to restore from it and validate the token
///    with a `whoami` call.
/// 2. On failure (or if no file exists), fall back to a fresh password login,
///    cleaning up the old store and device along the way.
async fn restore_or_login(config: &Config, session_file: &Path) -> Result<Client> {
    let mut old_device_id: Option<String> = None;

    // Try to restore from session.json
    if session_file.exists() {
        match try_restore_session(config, session_file).await {
            Ok(client) => return Ok(client),
            Err(e) => {
                warn!(
                    "Failed to restore session: {:#}. Will attempt a fresh login.",
                    e
                );
                // Remember the old device so we can clean it up later.
                old_device_id = read_old_device_id(session_file).await;

                // Delete the old session.json file, since it's now stale.
                if let Err(e) = tokio::fs::remove_file(session_file).await {
                    warn!("Failed to clean up old session file: {}", e);
                }
            }
        }
    } else if has_store_files(&config.state_store_path).await {
        // Store directory has data but no session.json – leftover from a crash
        // or manual deletion.
        bail!("State store exists but session.json is missing!")
    }

    // If we can't restore, then log in again.
    if !config.username.is_empty() {
        let client = login_fresh(config).await?;
        save_session_with_homeserver(config.homeserver_url.as_str(), &client, session_file).await?;

        // Best-effort: remove the device that we could no longer restore.
        if let Some(ref id) = old_device_id {
            try_delete_device(&client, config, id).await;
        }
        return Ok(client);
    }

    bail!("No usable credentials! Failed to restore session and failed to log in.");
}

/// Attempt to restore from `session.json`, then validate the token with a
/// `whoami` call.
async fn try_restore_session(config: &Config, session_file: &Path) -> Result<Client> {
    let content = tokio::fs::read_to_string(session_file)
        .await
        .context("Failed to read session.json")?;
    let saved: SavedSession =
        serde_json::from_str(&content).context("Failed to parse session.json")?;

    // Sanity-check: the saved homeserver must match the configured one.
    if let Some(ref saved_hs) = saved.homeserver {
        let a = saved_hs.trim_end_matches('/');
        let b = config.homeserver_url.as_str().trim_end_matches('/');
        if !a.eq_ignore_ascii_case(b) {
            anyhow::bail!(
                "Homeserver in session.json ({}) does not match configured homeserver ({})",
                a,
                b
            );
        }
    }

    // Build the client against the same sqlite store that was used when the
    // session was originally created.
    std::fs::create_dir_all(&config.state_store_path)?;
    let client = Client::builder()
        .homeserver_url(&config.homeserver_url)
        .sqlite_store(&config.state_store_path, None)
        .build()
        .await
        .context("Failed to build client for session restore")?;

    let user_id = matrix_sdk::ruma::UserId::parse(&saved.user_id)
        .context("Invalid user_id in session.json")?;

    let session = MatrixSession {
        meta: SessionMeta {
            user_id,
            device_id: saved.device_id.into(),
        },
        tokens: SessionTokens {
            access_token: saved.access_token,
            refresh_token: saved.refresh_token,
        },
    };

    client
        .matrix_auth()
        .restore_session(session, RoomLoadSettings::default())
        .await
        .context("restore_session() failed")?;

    // Validate the token is still accepted by the homeserver.
    client
        .whoami()
        .await
        .context("Session validation failed – access token may have been revoked")?;

    info!(
        "Restored session for {} (device {})",
        client.user_id().map(|u| u.to_string()).unwrap_or_default(),
        client
            .device_id()
            .map(|d| d.to_string())
            .unwrap_or_default(),
    );

    Ok(client)
}

/// Log in with username + password and return the newly authenticated client.
async fn login_fresh(config: &Config) -> Result<Client> {
    std::fs::create_dir_all(&config.state_store_path)?;

    let client = Client::builder()
        .homeserver_url(&config.homeserver_url)
        .sqlite_store(&config.state_store_path, None)
        .build()
        .await
        .context("Failed to build client")?;

    let password = config
        .password
        .as_deref()
        .context("Password is required for fresh login")?;

    client
        .matrix_auth()
        .login_username(&config.username, password)
        .initial_device_display_name("matrix-embed")
        .send()
        .await
        .context("Login failed")?;

    info!(
        "Logged in as {} (device {:?})",
        config.username,
        client.device_id().map(|d| d.to_string()),
    );

    Ok(client)
}

/// Persist the current Matrix session to `session.json` atomically.
async fn save_session(client: &Client, session_file: &Path) -> Result<()> {
    let session = client
        .matrix_auth()
        .session()
        .context("Client has no active session to save")?;

    let saved = SavedSession {
        homeserver: Some(client.homeserver().to_string()),
        user_id: session.meta.user_id.to_string(),
        device_id: session.meta.device_id.to_string(),
        access_token: session.tokens.access_token,
        refresh_token: session.tokens.refresh_token,
    };

    write_session_file(session_file, &saved).await
}

/// Like [`save_session`] but takes an explicit homeserver string (useful right
/// after login before the SDK may have resolved the URL via `.well-known`).
async fn save_session_with_homeserver(
    homeserver: &str,
    client: &Client,
    session_file: &Path,
) -> Result<()> {
    let session = client
        .matrix_auth()
        .session()
        .context("Client has no active session to save")?;

    let saved = SavedSession {
        homeserver: Some(homeserver.to_string()),
        user_id: session.meta.user_id.to_string(),
        device_id: session.meta.device_id.to_string(),
        access_token: session.tokens.access_token,
        refresh_token: session.tokens.refresh_token,
    };

    write_session_file(session_file, &saved).await
}

/// Write `SavedSession` to disk atomically (write to tmp + rename).
async fn write_session_file(session_file: &Path, saved: &SavedSession) -> Result<()> {
    let json = serde_json::to_string_pretty(saved)?;

    let tmp = session_file.with_extension("json.tmp");
    tokio::fs::write(&tmp, &json)
        .await
        .context("Failed to write temporary session file")?;
    tokio::fs::rename(&tmp, session_file)
        .await
        .context("Failed to rename temporary session file into place")?;

    info!("Session persisted to {}", session_file.display());
    Ok(())
}

/// Best-effort read of the `device_id` from an existing session file.
async fn read_old_device_id(session_file: &Path) -> Option<String> {
    let content = tokio::fs::read_to_string(session_file).await.ok()?;
    let saved: SavedSession = serde_json::from_str(&content).ok()?;
    Some(saved.device_id)
}

async fn has_store_files(state_store_path: &Path) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(state_store_path).await else {
        return false;
    };
    entries.next_entry().await.ok().flatten().is_some()
}

// ===========================================================================
// Verification / recovery
// ===========================================================================

/// Check whether our device is verified.  If it is not, attempt to recover
/// from the configured recovery passphrase so that we obtain the cross-signing
/// private keys without triggering a cryptographic reset.
async fn ensure_verified(client: &Client, config: &Config) {
    let verification_state = client.encryption().verification_state().get();
    info!("Current verification state: {:?}", verification_state);

    if verification_state == VerificationState::Verified {
        info!("Device is verified.");
        return;
    }

    // Log cross-signing key status for diagnostics.
    if let Some(status) = client.encryption().cross_signing_status().await {
        info!(
            "Cross-signing status: has_master={}, has_self_signing={}, has_user_signing={}, complete={}",
            status.has_master,
            status.has_self_signing,
            status.has_user_signing,
            status.is_complete(),
        );
        if status.is_complete() {
            // We have all three private keys locally; verification should
            // resolve after the next sync round-trip.
            info!(
                "All cross-signing keys are present locally; \
                 device should become verified after sync."
            );
            return;
        }
    } else {
        warn!("Could not query cross-signing status. (OLM machine not ready?)");
    }

    // Attempt recovery from the passphrase / recovery key.
    if let Some(ref passphrase) = config.recovery_passphrase {
        info!("Attempting to recover encryption state from recovery passphrase...");
        match client.encryption().recovery().recover(passphrase).await {
            Ok(()) => {
                info!("Recovery succeeded!");
                let new_state = client.encryption().verification_state().get();
                info!("Verification state after recovery: {:?}", new_state);
                if let Some(status) = client.encryption().cross_signing_status().await {
                    info!(
                        "Cross-signing after recovery: has_master={}, has_self_signing={}, has_user_signing={}",
                        status.has_master, status.has_self_signing, status.has_user_signing,
                    );
                }
                return;
            }
            Err(e) => {
                warn!("Recovery failed: {:#}", e);
            }
        }
    } else {
        info!("No --recovery-passphrase-file configured; skipping recovery attempt.");
    }

    warn!(
        "Device is NOT verified. Encrypted rooms may not work correctly. \
         Provide --recovery-passphrase-file or run `!embedbot admin reset-identity`."
    );
}

// ===========================================================================
// Session-change listener
// ===========================================================================

/// Spawn a background task that persists `session.json` whenever the SDK
/// reports that tokens have been refreshed or invalidated.
fn spawn_session_change_listener(client: &Client, session_file: std::path::PathBuf) {
    let mut receiver = client.subscribe_to_session_changes();
    let client = client.clone();

    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(change) => {
                    let description = format!("{:?}", change);
                    info!("Session change event: {}", description);

                    // Re-persist current session so the file on disk stays
                    // up-to-date (e.g. after a token refresh).
                    if let Err(e) = save_session(&client, &session_file).await {
                        error!(
                            "Failed to persist session after change ({}): {}",
                            description, e
                        );
                    }
                }
                Err(e) => {
                    // `RecvError::Lagged` → we missed messages, keep going.
                    // `RecvError::Closed` → the sender was dropped; stop.
                    let msg = e.to_string();
                    if msg.contains("closed") || msg.contains("channel closed") {
                        debug!("Session-change channel closed; stopping listener.");
                        break;
                    }
                    warn!("Session-change listener error: {}", msg);
                }
            }
        }
    });
}

// ===========================================================================
// Device helpers (used internally for old-device cleanup)
// ===========================================================================

/// Best-effort attempt to delete a single device from the account, handling
/// the UIAA password-auth flow if the server requires it.
pub(crate) async fn try_delete_device(client: &Client, config: &Config, device_id_str: &str) {
    // Never delete the device we are currently using.
    if let Some(current) = client.device_id() {
        if current.as_str() == device_id_str {
            return;
        }
    }

    let device_id: OwnedDeviceId = device_id_str.into();
    info!("Attempting to remove old device {}...", device_id);

    let devices = [device_id.clone()];
    match client.delete_devices(&devices, None).await {
        Ok(_) => {
            info!("Removed device {}", device_id);
        }
        Err(e) => {
            if let Some(uiaa_info) = e.as_uiaa_response() {
                let Some(password) = config.password.as_deref() else {
                    warn!(
                        "Server requires auth to delete device {} but no password is configured.",
                        device_id
                    );
                    return;
                };
                let mut auth = uiaa::Password::new(
                    uiaa::UserIdentifier::UserIdOrLocalpart(config.username.clone()),
                    password.to_owned(),
                );
                auth.session = uiaa_info.session.clone();

                match client
                    .delete_devices(&devices, Some(uiaa::AuthData::Password(auth)))
                    .await
                {
                    Ok(_) => info!("Removed device {} (with password auth)", device_id),
                    Err(e) => warn!("Failed to remove device {} with auth: {}", device_id, e),
                }
            } else {
                warn!("Failed to remove device {}: {}", device_id, e);
            }
        }
    }
}

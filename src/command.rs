use std::sync::Arc;

use crate::activitypub::ActivityPubDetector;
use crate::cas::MediaStore;
use crate::config::Config;
use crate::db::{CannedResponse, Database};
use crate::key_sharing;
use crate::metadata::Metadata;
use anyhow::{Context, Result, bail};
use matrix_sdk::Client;
use matrix_sdk::encryption::CrossSigningResetAuthType;
use matrix_sdk::ruma::api::client::uiaa;
use matrix_sdk::ruma::{OwnedDeviceId, RoomId};
use tracing::{error, info, warn};
use url::Url;

pub enum CommandResult {
    NotACommand,
    Response(String),
    KeyExport {
        passphrase: String,
        data: Vec<u8>,
        key_count: usize,
    },
    CannedResponse(CannedResponse),
}

pub async fn handle_command(
    body: &str,
    sender: &str,
    room_id: &str,
    config: &Config,
    client: &Client,
    database: &Arc<Database>,
    http_client: &reqwest::Client,
    media_store: &MediaStore,
    ap_detector: &ActivityPubDetector,
) -> CommandResult {
    let trimmed = body.trim();
    if trimmed.starts_with("!embedbot") {
        let args: Vec<&str> = trimmed.split_whitespace().collect();

        return match args.get(1).copied() {
            None => CommandResult::Response(usage_root()),
            Some("admin") => {
                handle_admin(
                    room_id,
                    &args[2..],
                    sender,
                    config,
                    client,
                    database,
                    http_client,
                    media_store,
                    ap_detector,
                )
                .await
            }
            Some("export-keys") => handle_export_keys(room_id, client, database).await,
            Some(other) => {
                CommandResult::Response(format!("Unknown command `{}`. {}", other, usage_root()))
            }
        };
    }

    // Check custom commands (message starts with !)
    if trimmed.starts_with('!') {
        if let Some(cmd_name) = trimmed.split_whitespace().next() {
            match database.get_custom_command(room_id, cmd_name).await {
                Ok(Some(response)) => return CommandResult::CannedResponse(response),
                Ok(None) => {}
                Err(e) => error!("Failed to look up custom command: {:?}", e),
            }
        }
    }

    CommandResult::NotACommand
}

fn usage_root() -> String {
    "Usage: `!embedbot <subcommand>`\n\n\
Available subcommands:\n\
- `export-keys` — Export room keys for this room (Element-compatible format)\n\
- `admin` — Admin commands (trusted users only)"
        .to_string()
}

fn usage_admin() -> String {
    "Usage: `!embedbot admin <subcommand>`\n\n\
Available subcommands:\n\
- `list-devices` — List all devices on this bot's account\n\
- `remove-device <device_id>` — Remove a device from this bot's account\n\
- `remove-other-devices` — Remove all devices except the current one\n\
- `reset-identity` — Reset cryptographic identity, set up recovery key and enable backups\n\
- `enable-key-sharing` — Enable automatic room key distribution in this room\n\
- `disable-key-sharing` — Disable automatic room key distribution in this room\n\
- `list-key-sharing` — List all rooms with key sharing enabled\n\
- `add-command [--global] <name> [media_url] [text...]` — Add/update a custom command\n\
- `remove-command [--global] <name>` — Remove a custom command\n\
- `list-commands [--global]` — List custom commands for this room (or globally)\n\
- `add-autoresponder [--global] <pattern> [probability] [media_url] [text...]` — Add/update an autoresponder\n\
- `remove-autoresponder [--global] <pattern>` — Remove an autoresponder\n\
- `list-autoresponders [--global]` — List autoresponders for this room (or globally)"
        .to_string()
}

/// Parses a `--global` flag from the front of args. Returns the effective
/// room_id (`""` when global) and the remaining args.
fn parse_global_flag<'a>(room_id: &'a str, args: &'a [&'a str]) -> (&'a str, &'a [&'a str]) {
    if args.first() == Some(&"--global") {
        ("", &args[1..])
    } else {
        (room_id, args)
    }
}

async fn handle_admin(
    room_id: &str,
    args: &[&str],
    sender: &str,
    config: &Config,
    client: &Client,
    database: &Arc<Database>,
    http_client: &reqwest::Client,
    media_store: &MediaStore,
    ap_detector: &ActivityPubDetector,
) -> CommandResult {
    if !config.trusted_users.iter().any(|u| u == sender) {
        warn!("Untrusted user {} attempted to use admin command", sender);
        return CommandResult::Response(
            "Permission denied. This command is restricted to trusted users.".to_string(),
        );
    }

    match args.first().copied() {
        None => CommandResult::Response(usage_admin()),
        Some("list-devices") => handle_list_devices(client).await,
        Some("remove-device") => handle_remove_device(&args[1..], config, client).await,
        Some("remove-other-devices") => handle_remove_other_devices(config, client).await,
        Some("reset-identity") => handle_reset_identity(config, client).await,
        Some("enable-key-sharing") => {
            handle_enable_key_sharing(room_id, &args[1..], database).await
        }
        Some("disable-key-sharing") => {
            handle_disable_key_sharing(room_id, &args[1..], database).await
        }
        Some("list-key-sharing") => handle_list_key_sharing(database).await,
        Some("add-command") => {
            handle_add_command(
                room_id,
                &args[1..],
                database,
                http_client,
                config,
                media_store,
                ap_detector,
            )
            .await
        }
        Some("remove-command") => handle_remove_command(room_id, &args[1..], database).await,
        Some("list-commands") => handle_list_commands(room_id, &args[1..], database).await,
        Some("add-autoresponder") => {
            handle_add_autoresponder(
                room_id,
                &args[1..],
                database,
                http_client,
                config,
                media_store,
                ap_detector,
            )
            .await
        }
        Some("remove-autoresponder") => {
            handle_remove_autoresponder(room_id, &args[1..], database).await
        }
        Some("list-autoresponders") => {
            handle_list_autoresponders(room_id, &args[1..], database).await
        }
        Some(other) => CommandResult::Response(format!(
            "Unknown admin command `{}`. {}",
            other,
            usage_admin()
        )),
    }
}

async fn handle_list_devices(client: &Client) -> CommandResult {
    info!("Admin request to list devices");

    match client.devices().await {
        Ok(response) => {
            let current_device_id = client.device_id().map(|d| d.to_string());
            let mut lines = vec!["**Devices on this account:**\n".to_string()];

            for device in &response.devices {
                let id = device.device_id.to_string();
                let name = device
                    .display_name
                    .as_deref()
                    .unwrap_or("(no display name)");
                let last_seen_ip = device.last_seen_ip.as_deref().unwrap_or("unknown");
                let is_current = current_device_id.as_deref() == Some(id.as_str());
                let marker = if is_current { " *(current)*" } else { "" };

                lines.push(format!(
                    "- `{}` — {}{} — last IP: {}",
                    id, name, marker, last_seen_ip,
                ));
            }

            CommandResult::Response(lines.join("\n"))
        }
        Err(e) => {
            warn!("Failed to list devices: {:?}", e);
            CommandResult::Response(format!("Failed to list devices: {}", e))
        }
    }
}

async fn handle_remove_device(args: &[&str], config: &Config, client: &Client) -> CommandResult {
    let Some(device_id_str) = args.first().copied() else {
        return CommandResult::Response(
            "Usage: `!embedbot admin remove-device <device_id>`".to_string(),
        );
    };

    let device_id: OwnedDeviceId = device_id_str.into();
    info!("Admin request to remove device {}", device_id);

    match remove_device(client, config, &device_id).await {
        Ok(()) => {
            info!("Successfully removed device {}", device_id);
            CommandResult::Response(format!("Device `{}` has been removed.", device_id))
        }
        Err(e) => {
            warn!("Failed to remove device {}: {:?}", device_id, e);
            CommandResult::Response(format!("Failed to remove device `{}`: {}", device_id, e))
        }
    }
}

async fn handle_remove_other_devices(config: &Config, client: &Client) -> CommandResult {
    info!("Admin request to remove all other devices");

    let current_device_id = match client.device_id() {
        Some(id) => id.to_owned(),
        None => {
            return CommandResult::Response("Cannot determine current device ID.".to_string());
        }
    };

    let devices_response = match client.devices().await {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to list devices: {:?}", e);
            return CommandResult::Response(format!("Failed to list devices: {}", e));
        }
    };

    let other_ids: Vec<OwnedDeviceId> = devices_response
        .devices
        .iter()
        .filter(|d| d.device_id != current_device_id)
        .map(|d| d.device_id.clone())
        .collect();

    if other_ids.is_empty() {
        return CommandResult::Response("No other devices to remove.".to_string());
    }

    let count = other_ids.len();
    info!("Removing {} other device(s)…", count);

    let mut removed = 0u32;
    let mut failed = 0u32;

    for device_id in &other_ids {
        match remove_device(client, config, device_id).await {
            Ok(()) => {
                info!("Removed device {}", device_id);
                removed += 1;
            }
            Err(e) => {
                error!("Failed to remove device {}: {}", device_id, e);
                failed += 1;
            }
        }
    }

    let mut msg = format!("Removed {} of {} other device(s).", removed, count);
    if failed > 0 {
        msg.push_str(&format!(" {} failed.", failed));
    }
    CommandResult::Response(msg)
}

async fn handle_reset_identity(config: &Config, client: &Client) -> CommandResult {
    info!("Admin request to reset cryptographic identity");

    match reset_identity(client, config).await {
        Ok(recovery_key) => {
            info!("Successfully reset identity and enabled recovery");
            CommandResult::Response(format!(
                "Cryptographic identity has been reset.\n\n**New recovery key:** `{}`",
                recovery_key
            ))
        }
        Err(e) => {
            warn!("Failed to reset identity: {:?}", e);
            CommandResult::Response(format!("Failed to reset identity: {}", e))
        }
    }
}

/// Resets the bot's cryptographic identity, then sets up a recovery key and
/// enables backups.
async fn reset_identity(client: &Client, config: &Config) -> Result<String> {
    let handle = client
        .encryption()
        .recovery()
        .reset_identity()
        .await
        .context("Failed to reset identity")?;

    if let Some(handle) = handle {
        match handle.auth_type() {
            CrossSigningResetAuthType::Uiaa(uiaa_info) => {
                let password = config
                    .password
                    .as_deref()
                    .context("Server requires interactive auth to reset cross-signing keys, but no password is configured")?;

                let mut auth = uiaa::Password::new(
                    uiaa::UserIdentifier::UserIdOrLocalpart(config.username.clone()),
                    password.to_owned(),
                );
                auth.session = uiaa_info.session.clone();

                handle
                    .reset(Some(uiaa::AuthData::Password(auth)))
                    .await
                    .context("Failed to authenticate cross-signing reset")?;
            }
            other => bail!(
                "Server requires unsupported authentication method for cross-signing reset: {:?}",
                other
            ),
        }
    }

    // Step 2 — create backup + recovery key
    let recovery_key = client
        .encryption()
        .recovery()
        .enable()
        .await
        .context("Failed to enable recovery and backups")?;

    Ok(recovery_key)
}

async fn handle_export_keys(
    room_id: &str,
    client: &Client,
    database: &Arc<Database>,
) -> CommandResult {
    // 1. Check that key sharing is enabled for this room.
    match database.is_key_sharing_enabled(room_id).await {
        Ok(false) => {
            return CommandResult::Response(
                "Key export is not available for this room.\n\n\
                 An admin must first enable it with \
                 `!embedbot admin enable-key-sharing <room_id>`."
                    .to_string(),
            );
        }
        Err(e) => {
            error!(
                "Failed to check key-sharing status for {}: {:?}",
                room_id, e
            );
            return CommandResult::Response(format!("Failed to check key-sharing status: {}", e));
        }
        Ok(true) => {}
    }

    // 2. Parse the room ID.
    let room_id = match RoomId::parse(room_id) {
        Ok(id) => id,
        Err(e) => {
            error!("Invalid room ID in export-keys handler: {}", e);
            return CommandResult::Response("Internal error: invalid room ID.".to_string());
        }
    };

    // 3. Perform the export.
    info!(%room_id, "Handling export-keys command");

    match key_sharing::export_room_keys(client, &room_id).await {
        Ok(Some(export)) => CommandResult::KeyExport {
            passphrase: export.passphrase,
            data: export.data,
            key_count: export.key_count,
        },
        Ok(None) => CommandResult::Response(
            "There are no exportable room keys for this room yet.".to_string(),
        ),
        Err(e) => {
            error!(%room_id, "Failed to export room keys: {:?}", e);
            CommandResult::Response(format!("Failed to export room keys: {}", e))
        }
    }
}

async fn handle_enable_key_sharing(
    mut room_id: &str,
    args: &[&str],
    database: &Arc<Database>,
) -> CommandResult {
    if let Some(room_id_arg) = args.first().copied() {
        room_id = room_id_arg;
    }

    info!("Admin request to enable key sharing for room {}", room_id);

    match database.enable_key_sharing(room_id).await {
        Ok(()) => CommandResult::Response(format!(
            "Room key sharing has been **enabled** for `{}`.",
            room_id
        )),
        Err(e) => {
            error!("Failed to enable key sharing for {}: {:?}", room_id, e);
            CommandResult::Response(format!("Failed to enable key sharing: {}", e))
        }
    }
}

async fn handle_disable_key_sharing(
    mut room_id: &str,
    args: &[&str],
    database: &Arc<Database>,
) -> CommandResult {
    if let Some(room_id_arg) = args.first().copied() {
        room_id = room_id_arg;
    }

    info!("Admin request to disable key sharing for room {}", room_id);

    match database.disable_key_sharing(room_id).await {
        Ok(()) => CommandResult::Response(format!(
            "Room key sharing has been **disabled** for `{}`.",
            room_id
        )),
        Err(e) => {
            error!("Failed to disable key sharing for {}: {:?}", room_id, e);
            CommandResult::Response(format!("Failed to disable key sharing: {}", e))
        }
    }
}

async fn handle_list_key_sharing(database: &Arc<Database>) -> CommandResult {
    info!("Admin request to list key-sharing rooms");

    match database.list_key_sharing_rooms().await {
        Ok(rooms) if rooms.is_empty() => {
            CommandResult::Response("No rooms have key sharing enabled.".to_string())
        }
        Ok(rooms) => {
            let mut lines = vec![format!(
                "**Rooms with key sharing enabled ({}):**\n",
                rooms.len()
            )];
            for room_id in &rooms {
                lines.push(format!("- `{}`", room_id));
            }
            CommandResult::Response(lines.join("\n"))
        }
        Err(e) => {
            error!("Failed to list key-sharing rooms: {:?}", e);
            CommandResult::Response(format!("Failed to list key-sharing rooms: {}", e))
        }
    }
}

async fn fetch_and_store_media(
    url_str: &str,
    http_client: &reqwest::Client,
    config: &Config,
    media_store: &MediaStore,
    ap_detector: &ActivityPubDetector,
) -> Result<(String, String, String)> {
    let url = Url::parse(url_str).context("Invalid URL")?;
    let meta = Metadata::fetch_from_url(http_client, &url, ap_detector).await?;
    let media_url = meta
        .video_url
        .or(meta.audio_url)
        .or(meta.image_url)
        .unwrap_or_else(|| url.clone());

    let response = http_client
        .get(media_url.clone())
        .timeout(config.download_timeout)
        .send()
        .await
        .context("Failed to download media")?
        .error_for_status()
        .context("Media download returned error status")?;

    let mime_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .trim()
        .to_string();

    let filename = media_url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .unwrap_or("media")
        .to_string();

    let data = response.bytes().await.context("Failed to read media")?;
    let hash = media_store.store(&data).await?;
    Ok((hash, filename, mime_type))
}

async fn handle_add_command(
    room_id: &str,
    args: &[&str],
    database: &Arc<Database>,
    http_client: &reqwest::Client,
    config: &Config,
    media_store: &MediaStore,
    ap_detector: &ActivityPubDetector,
) -> CommandResult {
    let (room_id, args) = parse_global_flag(room_id, args);
    let scope = if room_id.is_empty() { " globally" } else { "" };

    let Some(name) = args.first().copied() else {
        return CommandResult::Response(
            "Usage: `!embedbot admin add-command [--global] <name> [media_url] [text...]`"
                .to_string(),
        );
    };

    if !name.starts_with('!') {
        return CommandResult::Response("Command name must start with `!`.".to_string());
    }

    let mut rest = &args[1..];
    let mut media_info = None;

    if let Some(first) = rest.first() {
        if first.starts_with("http://") || first.starts_with("https://") {
            match fetch_and_store_media(first, http_client, config, media_store, ap_detector).await
            {
                Ok(info) => media_info = Some(info),
                Err(e) => {
                    return CommandResult::Response(format!("Failed to fetch media: {}", e));
                }
            }
            rest = &rest[1..];
        }
    }

    let text = if rest.is_empty() {
        None
    } else {
        Some(rest.join(" "))
    };

    if text.is_none() && media_info.is_none() {
        return CommandResult::Response(
            "Must provide at least text or a media URL.\n\n\
             Usage: `!embedbot admin add-command <name> [media_url] [text...]`"
                .to_string(),
        );
    }

    let (cas_hash, filename, mime_type) = match &media_info {
        Some((h, f, m)) => (Some(h.as_str()), Some(f.as_str()), Some(m.as_str())),
        None => (None, None, None),
    };

    let response_id = match database
        .create_canned_response(text.as_deref(), cas_hash, filename, mime_type)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            error!("Failed to create canned response: {:?}", e);
            return CommandResult::Response(format!("Failed to create response: {}", e));
        }
    };

    match database
        .add_custom_command(room_id, name, response_id)
        .await
    {
        Ok(()) => CommandResult::Response(format!(
            "Custom command `{}` has been set{}{}.",
            name,
            scope,
            if media_info.is_some() {
                " (with media)"
            } else {
                ""
            }
        )),
        Err(e) => {
            error!("Failed to add custom command: {:?}", e);
            CommandResult::Response(format!("Failed to add command: {}", e))
        }
    }
}

async fn handle_remove_command(
    room_id: &str,
    args: &[&str],
    database: &Arc<Database>,
) -> CommandResult {
    let (room_id, args) = parse_global_flag(room_id, args);
    let scope = if room_id.is_empty() {
        "globally"
    } else {
        "in this room"
    };

    let Some(name) = args.first().copied() else {
        return CommandResult::Response(
            "Usage: `!embedbot admin remove-command [--global] <name>`".to_string(),
        );
    };

    match database.remove_custom_command(room_id, name).await {
        Ok(true) => CommandResult::Response(format!("Custom command `{}` has been removed.", name)),
        Ok(false) => {
            CommandResult::Response(format!("No custom command `{}` found {}.", name, scope))
        }
        Err(e) => {
            error!("Failed to remove custom command: {:?}", e);
            CommandResult::Response(format!("Failed to remove command: {}", e))
        }
    }
}

async fn handle_list_commands(
    room_id: &str,
    args: &[&str],
    database: &Arc<Database>,
) -> CommandResult {
    let (room_id, _args) = parse_global_flag(room_id, args);
    let scope = if room_id.is_empty() {
        "globally"
    } else {
        "for this room"
    };

    match database.list_custom_commands(room_id).await {
        Ok(cmds) if cmds.is_empty() => {
            CommandResult::Response(format!("No custom commands configured {}.", scope))
        }
        Ok(cmds) => {
            let mut lines = vec![format!("**Custom commands {} ({}):**\n", scope, cmds.len())];
            for cmd in &cmds {
                let has_media = cmd.response.media_cas_hash.is_some();
                let text_preview = cmd.response.text_markdown.as_deref().unwrap_or("(no text)");
                let truncated = if text_preview.len() > 50 {
                    format!("{}…", &text_preview[..50])
                } else {
                    text_preview.to_string()
                };
                lines.push(format!(
                    "- `{}` — {}{}",
                    cmd.command_name,
                    truncated,
                    if has_media { " 📎" } else { "" }
                ));
            }
            CommandResult::Response(lines.join("\n"))
        }
        Err(e) => {
            error!("Failed to list custom commands: {:?}", e);
            CommandResult::Response(format!("Failed to list commands: {}", e))
        }
    }
}

async fn handle_add_autoresponder(
    room_id: &str,
    args: &[&str],
    database: &Arc<Database>,
    http_client: &reqwest::Client,
    config: &Config,
    media_store: &MediaStore,
    ap_detector: &ActivityPubDetector,
) -> CommandResult {
    let (room_id, args) = parse_global_flag(room_id, args);
    let scope = if room_id.is_empty() { " globally" } else { "" };

    let Some(pattern) = args.first().copied() else {
        return CommandResult::Response(
            "Usage: `!embedbot admin add-autoresponder [--global] <pattern> [probability] [media_url] [text...]`"
                .to_string(),
        );
    };

    if regex::Regex::new(pattern).is_err() {
        return CommandResult::Response(format!("`{}` is not a valid regex pattern.", pattern));
    }

    let mut rest = &args[1..];
    let mut probability = 1.0;

    if let Some(first) = rest.first() {
        if let Ok(p) = first.parse::<f64>() {
            if (0.0..=1.0).contains(&p) {
                probability = p;
                rest = &rest[1..];
            }
        }
    }

    let mut media_info = None;
    if let Some(first) = rest.first() {
        if first.starts_with("http://") || first.starts_with("https://") {
            match fetch_and_store_media(first, http_client, config, media_store, ap_detector).await
            {
                Ok(info) => media_info = Some(info),
                Err(e) => {
                    return CommandResult::Response(format!("Failed to fetch media: {}", e));
                }
            }
            rest = &rest[1..];
        }
    }

    let text = if rest.is_empty() {
        None
    } else {
        Some(rest.join(" "))
    };

    if text.is_none() && media_info.is_none() {
        return CommandResult::Response(
            "Must provide at least text or a media URL.\n\n\
             Usage: `!embedbot admin add-autoresponder <pattern> [probability] [media_url] [text...]`"
                .to_string(),
        );
    }

    let (cas_hash, filename, mime_type) = match &media_info {
        Some((h, f, m)) => (Some(h.as_str()), Some(f.as_str()), Some(m.as_str())),
        None => (None, None, None),
    };

    let response_id = match database
        .create_canned_response(text.as_deref(), cas_hash, filename, mime_type)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            error!("Failed to create canned response: {:?}", e);
            return CommandResult::Response(format!("Failed to create response: {}", e));
        }
    };

    match database
        .add_autoresponder(room_id, pattern, probability, response_id)
        .await
    {
        Ok(()) => {
            let prob_str = if probability < 1.0 {
                format!(" ({}% chance)", (probability * 100.0) as u32)
            } else {
                String::new()
            };
            CommandResult::Response(format!(
                "Autoresponder for `{}`{} has been set{}{}.",
                pattern,
                prob_str,
                scope,
                if media_info.is_some() {
                    " (with media)"
                } else {
                    ""
                }
            ))
        }
        Err(e) => {
            error!("Failed to add autoresponder: {:?}", e);
            CommandResult::Response(format!("Failed to add autoresponder: {}", e))
        }
    }
}

async fn handle_remove_autoresponder(
    room_id: &str,
    args: &[&str],
    database: &Arc<Database>,
) -> CommandResult {
    let (room_id, args) = parse_global_flag(room_id, args);
    let scope = if room_id.is_empty() {
        "globally"
    } else {
        "in this room"
    };

    let Some(pattern) = args.first().copied() else {
        return CommandResult::Response(
            "Usage: `!embedbot admin remove-autoresponder [--global] <pattern>`".to_string(),
        );
    };

    match database.remove_autoresponder(room_id, pattern).await {
        Ok(true) => {
            CommandResult::Response(format!("Autoresponder for `{}` has been removed.", pattern))
        }
        Ok(false) => CommandResult::Response(format!(
            "No autoresponder for `{}` found {}.",
            pattern, scope
        )),
        Err(e) => {
            error!("Failed to remove autoresponder: {:?}", e);
            CommandResult::Response(format!("Failed to remove autoresponder: {}", e))
        }
    }
}

async fn handle_list_autoresponders(
    room_id: &str,
    args: &[&str],
    database: &Arc<Database>,
) -> CommandResult {
    let (room_id, _args) = parse_global_flag(room_id, args);
    let scope = if room_id.is_empty() {
        "globally"
    } else {
        "for this room"
    };

    match database.list_autoresponders(room_id).await {
        Ok(autos) if autos.is_empty() => {
            CommandResult::Response(format!("No autoresponders configured {}.", scope))
        }
        Ok(autos) => {
            let mut lines = vec![format!("**Autoresponders {} ({}):**\n", scope, autos.len())];
            for auto in &autos {
                let has_media = auto.response.media_cas_hash.is_some();
                let text_preview = auto
                    .response
                    .text_markdown
                    .as_deref()
                    .unwrap_or("(no text)");
                let truncated = if text_preview.len() > 50 {
                    format!("{}…", &text_preview[..50])
                } else {
                    text_preview.to_string()
                };
                let prob_str = if auto.probability < 1.0 {
                    format!(" ({}%)", (auto.probability * 100.0) as u32)
                } else {
                    String::new()
                };
                lines.push(format!(
                    "- `{}`{} — {}{}",
                    auto.pattern,
                    prob_str,
                    truncated,
                    if has_media { " 📎" } else { "" }
                ));
            }
            CommandResult::Response(lines.join("\n"))
        }
        Err(e) => {
            error!("Failed to list autoresponders: {:?}", e);
            CommandResult::Response(format!("Failed to list autoresponders: {}", e))
        }
    }
}

/// Check autoresponders for a room. Returns the first matching canned response
/// (after rolling against the configured probability), or `None`.
pub async fn check_autoresponders(
    body: &str,
    room_id: &str,
    database: &Database,
) -> Option<CannedResponse> {
    let autoresponders = match database.get_autoresponders(room_id).await {
        Ok(a) => a,
        Err(e) => {
            error!("Failed to get autoresponders: {:?}", e);
            return None;
        }
    };

    for auto in autoresponders {
        let re = match regex::Regex::new(&auto.pattern) {
            Ok(re) => re,
            Err(e) => {
                warn!("Invalid autoresponder pattern '{}': {}", auto.pattern, e);
                continue;
            }
        };
        if re.is_match(body) {
            if auto.probability < 1.0 {
                use rand::Rng;
                if rand::thread_rng().r#gen::<f64>() >= auto.probability {
                    continue;
                }
            }
            return Some(auto.response);
        }
    }

    None
}

pub(crate) async fn remove_device(
    client: &Client,
    config: &Config,
    device_id: &OwnedDeviceId,
) -> Result<()> {
    let devices = [device_id.clone()];

    match client.delete_devices(&devices, None).await {
        Ok(_) => Ok(()),
        Err(e) => {
            // If the server requires interactive auth, handle the UIAA flow.
            if let Some(info) = e.as_uiaa_response() {
                let password = config
                    .password
                    .as_deref()
                    .context("Server requires interactive auth to delete devices, but no password is configured")?;

                let mut auth = uiaa::Password::new(
                    uiaa::UserIdentifier::UserIdOrLocalpart(config.username.clone()),
                    password.to_owned(),
                );
                auth.session = info.session.clone();

                client
                    .delete_devices(&devices, Some(uiaa::AuthData::Password(auth)))
                    .await
                    .context("Failed to delete device with password auth")?;

                return Ok(());
            }

            bail!("Failed to delete device: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(trusted: Vec<&str>) -> Config {
        Config {
            trusted_users: trusted.into_iter().map(String::from).collect(),
            ..Config::default()
        }
    }

    async fn test_database() -> Arc<Database> {
        Arc::new(Database::open_in_memory().await.unwrap())
    }

    async fn run_cmd(
        body: &str,
        sender: &str,
        room_id: &str,
        config: &Config,
        client: &Client,
        database: &Arc<Database>,
    ) -> CommandResult {
        let dir = tempfile::TempDir::new().unwrap();
        let media_store = crate::cas::MediaStore::open(dir.path()).await.unwrap();
        let http_client = reqwest::Client::new();
        let ap_detector = crate::activitypub::ActivityPubDetector::new();
        handle_command(
            body,
            sender,
            room_id,
            config,
            client,
            database,
            &http_client,
            &media_store,
            &ap_detector,
        )
        .await
    }

    #[tokio::test]
    async fn test_not_a_command() {
        let config = test_config(vec![]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "hello world",
            "@user:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        assert!(matches!(result, CommandResult::NotACommand));
    }

    #[tokio::test]
    async fn test_not_a_command_url() {
        let config = test_config(vec![]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "https://example.com",
            "@user:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        assert!(matches!(result, CommandResult::NotACommand));
    }

    #[tokio::test]
    async fn test_base_command_shows_help() {
        let config = test_config(vec![]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot",
            "@user:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("Usage")),
            other => panic!(
                "Expected Response, got {:?}",
                matches!(other, CommandResult::NotACommand)
            ),
        }
    }

    #[tokio::test]
    async fn test_custom_command_trigger() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        // Set up a custom command via DB directly.
        let rid = db
            .create_canned_response(Some("Here are useful links!"), None, None, None)
            .await
            .unwrap();
        db.add_custom_command("!testroom:example.com", "!links", rid)
            .await
            .unwrap();

        // Should trigger the custom command.
        let result = run_cmd(
            "!links",
            "@user:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::CannedResponse(cr) => {
                assert_eq!(cr.text_markdown.as_deref(), Some("Here are useful links!"));
            }
            _ => panic!("Expected CannedResponse"),
        }

        // Different room should not match.
        let result = run_cmd(
            "!links",
            "@user:example.com",
            "!otherroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        assert!(matches!(result, CommandResult::NotACommand));

        // Random non-command text should not match.
        let result = run_cmd(
            "hello",
            "@user:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        assert!(matches!(result, CommandResult::NotACommand));
    }

    #[tokio::test]
    async fn test_admin_add_and_remove_command() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        // add-command with text only
        let result = run_cmd(
            "!embedbot admin add-command !greet Hello, world!",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match &result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("has been set"), "got: {}", msg);
                assert!(msg.contains("!greet"));
            }
            _ => panic!("Expected Response"),
        }

        // Verify the command exists.
        let cr = db
            .get_custom_command("!testroom:example.com", "!greet")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cr.text_markdown.as_deref(), Some("Hello, world!"));

        // remove-command
        let result = run_cmd(
            "!embedbot admin remove-command !greet",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("removed")),
            _ => panic!("Expected Response"),
        }

        assert!(
            db.get_custom_command("!testroom:example.com", "!greet")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_admin_add_command_missing_args() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin add-command",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("Usage")),
            _ => panic!("Expected Response"),
        }

        // Name without text or media
        let result = run_cmd(
            "!embedbot admin add-command !test",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("Must provide")),
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_admin_add_command_no_bang() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin add-command greet Hello",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("must start with `!`")),
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_admin_list_commands() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        // Empty list
        let result = run_cmd(
            "!embedbot admin list-commands",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("No custom commands")),
            _ => panic!("Expected Response"),
        }

        // Add one and list
        let rid = db
            .create_canned_response(Some("hi"), None, None, None)
            .await
            .unwrap();
        db.add_custom_command("!testroom:example.com", "!hi", rid)
            .await
            .unwrap();

        let result = run_cmd(
            "!embedbot admin list-commands",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("!hi"));
                assert!(msg.contains("1"));
            }
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_admin_add_and_remove_autoresponder() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin add-autoresponder hello 0.5 hi there!",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match &result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("has been set"), "got: {}", msg);
                assert!(msg.contains("50%"));
            }
            _ => panic!("Expected Response"),
        }

        let autos = db
            .get_autoresponders("!testroom:example.com")
            .await
            .unwrap();
        assert_eq!(autos.len(), 1);
        assert_eq!(autos[0].pattern, "hello");
        assert_eq!(autos[0].probability, 0.5);
        assert_eq!(
            autos[0].response.text_markdown.as_deref(),
            Some("hi there!")
        );

        // list
        let result = run_cmd(
            "!embedbot admin list-autoresponders",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("hello"));
                assert!(msg.contains("50%"));
            }
            _ => panic!("Expected Response"),
        }

        // remove
        let result = run_cmd(
            "!embedbot admin remove-autoresponder hello",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("removed")),
            _ => panic!("Expected Response"),
        }

        assert!(
            db.get_autoresponders("!testroom:example.com")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_admin_add_autoresponder_invalid_regex() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin add-autoresponder [invalid response text",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("not a valid regex")),
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_check_autoresponders_basic() {
        let db = test_database().await;
        let room = "!testroom:example.com";

        let rid = db
            .create_canned_response(Some("world!"), None, None, None)
            .await
            .unwrap();
        db.add_autoresponder(room, "hello", 1.0, rid).await.unwrap();

        let result = check_autoresponders("hello world", room, &db).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().text_markdown.as_deref(), Some("world!"));

        let result = check_autoresponders("goodbye", room, &db).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_check_autoresponders_probability_zero() {
        let db = test_database().await;
        let room = "!testroom:example.com";

        let rid = db
            .create_canned_response(Some("nope"), None, None, None)
            .await
            .unwrap();
        db.add_autoresponder(room, "hi", 0.0, rid).await.unwrap();

        // With probability 0, should never trigger.
        for _ in 0..20 {
            assert!(check_autoresponders("hi", room, &db).await.is_none());
        }
    }

    #[tokio::test]
    async fn test_admin_add_global_command() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin add-command --global !greet Hello global!",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match &result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("has been set"), "got: {}", msg);
                assert!(msg.contains("globally"));
            }
            _ => panic!("Expected Response"),
        }

        // Should be visible from any room.
        let result = run_cmd(
            "!greet",
            "@user:example.com",
            "!anyroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::CannedResponse(cr) => {
                assert_eq!(cr.text_markdown.as_deref(), Some("Hello global!"));
            }
            _ => panic!("Expected CannedResponse"),
        }

        // Room-specific override takes priority.
        let result = run_cmd(
            "!embedbot admin add-command !greet Hello room!",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        assert!(matches!(result, CommandResult::Response(_)));

        let result = run_cmd(
            "!greet",
            "@user:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::CannedResponse(cr) => {
                assert_eq!(cr.text_markdown.as_deref(), Some("Hello room!"));
            }
            _ => panic!("Expected CannedResponse"),
        }

        // Other rooms still see global.
        let result = run_cmd(
            "!greet",
            "@user:example.com",
            "!other:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::CannedResponse(cr) => {
                assert_eq!(cr.text_markdown.as_deref(), Some("Hello global!"));
            }
            _ => panic!("Expected CannedResponse"),
        }

        // list-commands --global shows the global command.
        let result = run_cmd(
            "!embedbot admin list-commands --global",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("!greet"));
                assert!(msg.contains("globally"));
            }
            _ => panic!("Expected Response"),
        }

        // remove --global
        let result = run_cmd(
            "!embedbot admin remove-command --global !greet",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("removed")),
            _ => panic!("Expected Response"),
        }

        // Still have the room-specific one.
        let result = run_cmd(
            "!greet",
            "@user:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        assert!(matches!(result, CommandResult::CannedResponse(_)));
    }

    #[tokio::test]
    async fn test_admin_add_global_autoresponder() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin add-autoresponder --global hello 0.5 hi there!",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match &result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("has been set"), "got: {}", msg);
                assert!(msg.contains("globally"));
                assert!(msg.contains("50%"));
            }
            _ => panic!("Expected Response"),
        }

        // Should be visible from any room via check_autoresponders.
        let result = check_autoresponders("hello world", "!anyroom:example.com", &db).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().text_markdown.as_deref(), Some("hi there!"));

        // list-autoresponders --global
        let result = run_cmd(
            "!embedbot admin list-autoresponders --global",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("hello"));
                assert!(msg.contains("globally"));
            }
            _ => panic!("Expected Response"),
        }

        // remove --global
        let result = run_cmd(
            "!embedbot admin remove-autoresponder --global hello",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("removed")),
            _ => panic!("Expected Response"),
        }

        let result = check_autoresponders("hello world", "!anyroom:example.com", &db).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_admin_help_includes_new_commands() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("add-command"));
                assert!(msg.contains("remove-command"));
                assert!(msg.contains("list-commands"));
                assert!(msg.contains("add-autoresponder"));
                assert!(msg.contains("remove-autoresponder"));
                assert!(msg.contains("list-autoresponders"));
            }
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_unknown_subcommand() {
        let config = test_config(vec![]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot foobar",
            "@user:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("Unknown command"));
                assert!(msg.contains("foobar"));
            }
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_admin_untrusted_user() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin remove-device FOOBAR",
            "@random:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("Permission denied")),
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_admin_help() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("Usage"));
                assert!(msg.contains("enable-key-sharing"));
                assert!(msg.contains("disable-key-sharing"));
                assert!(msg.contains("list-key-sharing"));
            }
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_admin_unknown_subcommand() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin nope",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("Unknown admin command"));
                assert!(msg.contains("nope"));
            }
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_admin_remove_device_missing_id() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin remove-device",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("Usage")),
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_admin_reset_identity_untrusted() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin reset-identity",
            "@random:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("Permission denied")),
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_admin_enable_key_sharing() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin enable-key-sharing !test:example.com",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("enabled"));
                assert!(msg.contains("!test:example.com"));
            }
            _ => panic!("Expected Response"),
        }

        // Verify it was actually persisted.
        assert!(
            db.is_key_sharing_enabled("!test:example.com")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_admin_enable_key_sharing_bad_room_id() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin enable-key-sharing not-a-room-id",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("does not look like a valid room ID"));
            }
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_admin_enable_key_sharing_missing_arg() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin enable-key-sharing",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("Usage")),
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_admin_disable_key_sharing() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        db.enable_key_sharing("!test:example.com").await.unwrap();

        let result = run_cmd(
            "!embedbot admin disable-key-sharing !test:example.com",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("disabled"));
                assert!(msg.contains("!test:example.com"));
            }
            _ => panic!("Expected Response"),
        }

        assert!(
            !db.is_key_sharing_enabled("!test:example.com")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_admin_list_key_sharing_empty() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot admin list-key-sharing",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("No rooms have key sharing enabled"));
            }
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_export_keys_not_enabled() {
        let config = test_config(vec![]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        // Key sharing is NOT enabled for this room.
        let result = run_cmd(
            "!embedbot export-keys",
            "@random:example.com",
            "!myroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("not available"));
                assert!(msg.contains("enable-key-sharing"));
            }
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_export_keys_enabled_no_olm() {
        let config = test_config(vec![]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        // Enable key sharing for the room.
        db.enable_key_sharing("!myroom:example.com").await.unwrap();

        // The test Client has no OlmMachine, so the export will fail
        // gracefully with an error message.
        let result = run_cmd(
            "!embedbot export-keys",
            "@random:example.com",
            "!myroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("Failed to export room keys"));
            }
            _ => panic!("Expected Response with error"),
        }
    }

    #[tokio::test]
    async fn test_export_keys_shows_in_help() {
        let config = test_config(vec![]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = run_cmd(
            "!embedbot",
            "@user:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("export-keys"));
            }
            _ => panic!("Expected Response"),
        }
    }

    #[tokio::test]
    async fn test_admin_list_key_sharing_with_rooms() {
        let config = test_config(vec!["@admin:example.com"]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        db.enable_key_sharing("!room1:example.com").await.unwrap();
        db.enable_key_sharing("!room2:example.com").await.unwrap();

        let result = run_cmd(
            "!embedbot admin list-key-sharing",
            "@admin:example.com",
            "!testroom:example.com",
            &config,
            &client,
            &db,
        )
        .await;
        match result {
            CommandResult::Response(msg) => {
                assert!(msg.contains("!room1:example.com"));
                assert!(msg.contains("!room2:example.com"));
                assert!(msg.contains("2"));
            }
            _ => panic!("Expected Response"),
        }
    }
}

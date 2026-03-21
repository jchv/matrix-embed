use std::sync::Arc;

use crate::config::Config;
use crate::db::Database;
use crate::key_sharing;
use anyhow::{Context, Result, bail};
use matrix_sdk::Client;
use matrix_sdk::encryption::CrossSigningResetAuthType;
use matrix_sdk::ruma::api::client::uiaa;
use matrix_sdk::ruma::{OwnedDeviceId, RoomId};
use tracing::{error, info, warn};

pub enum CommandResult {
    NotACommand,
    Response(String),
    KeyExport {
        passphrase: String,
        data: Vec<u8>,
        key_count: usize,
    },
}

pub async fn handle_command(
    body: &str,
    sender: &str,
    room_id: &str,
    config: &Config,
    client: &Client,
    database: &Arc<Database>,
) -> CommandResult {
    let trimmed = body.trim();
    if !trimmed.starts_with("!embedbot") {
        return CommandResult::NotACommand;
    }

    let args: Vec<&str> = trimmed.split_whitespace().collect();

    // args[0] is "!embedbot"
    match args.get(1).copied() {
        None => CommandResult::Response(usage_root()),
        Some("admin") => handle_admin(room_id, &args[2..], sender, config, client, database).await,
        Some("export-keys") => handle_export_keys(room_id, client, database).await,
        Some(other) => {
            CommandResult::Response(format!("Unknown command `{}`. {}", other, usage_root()))
        }
    }
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
- `list-key-sharing` — List all rooms with key sharing enabled"
        .to_string()
}

async fn handle_admin(
    room_id: &str,
    args: &[&str],
    sender: &str,
    config: &Config,
    client: &Client,
    database: &Arc<Database>,
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

    #[tokio::test]
    async fn test_not_a_command() {
        let config = test_config(vec![]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = handle_command(
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

        let result = handle_command(
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

        let result = handle_command(
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
    async fn test_unknown_subcommand() {
        let config = test_config(vec![]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();
        let db = test_database().await;

        let result = handle_command(
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

        let result = handle_command(
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

        let result = handle_command(
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

        let result = handle_command(
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

        let result = handle_command(
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

        let result = handle_command(
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

        let result = handle_command(
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

        let result = handle_command(
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

        let result = handle_command(
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

        let result = handle_command(
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

        let result = handle_command(
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
        let result = handle_command(
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
        let result = handle_command(
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

        let result = handle_command(
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

        let result = handle_command(
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

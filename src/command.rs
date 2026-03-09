use crate::config::Config;
use anyhow::{Context, Result, bail};
use matrix_sdk::Client;
use matrix_sdk::encryption::CrossSigningResetAuthType;
use matrix_sdk::ruma::OwnedDeviceId;
use matrix_sdk::ruma::api::client::uiaa;
use tracing::{info, warn};

pub enum CommandResult {
    NotACommand,
    Response(String),
}

pub async fn handle_command(
    body: &str,
    sender: &str,
    config: &Config,
    client: &Client,
) -> CommandResult {
    let trimmed = body.trim();
    if !trimmed.starts_with("!embedbot") {
        return CommandResult::NotACommand;
    }

    let args: Vec<&str> = trimmed.split_whitespace().collect();

    // args[0] is "!embedbot"
    match args.get(1).copied() {
        None => CommandResult::Response(usage_root()),
        Some("admin") => handle_admin(&args[2..], sender, config, client).await,
        Some(other) => {
            CommandResult::Response(format!("Unknown command `{}`. {}", other, usage_root()))
        }
    }
}

fn usage_root() -> String {
    "Usage: `!embedbot <subcommand>`\n\n\
Available subcommands:\n\
- `admin` — Admin commands (trusted users only)"
        .to_string()
}

fn usage_admin() -> String {
    "Usage: `!embedbot admin <subcommand>`\n\n\
Available subcommands:\n\
- `remove-device <device_id>` — Remove a device from this bot's account\n\
- `reset-identity` — Reset cryptographic identity, set up recovery key and enable backups"
        .to_string()
}

async fn handle_admin(
    args: &[&str],
    sender: &str,
    config: &Config,
    client: &Client,
) -> CommandResult {
    if !config.trusted_users.iter().any(|u| u == sender) {
        warn!("Untrusted user {} attempted to use admin command", sender);
        return CommandResult::Response(
            "Permission denied. This command is restricted to trusted users.".to_string(),
        );
    }

    match args.first().copied() {
        None => CommandResult::Response(usage_admin()),
        Some("remove-device") => handle_remove_device(&args[1..], config, client).await,
        Some("reset-identity") => handle_reset_identity(config, client).await,
        Some(other) => CommandResult::Response(format!(
            "Unknown admin command `{}`. {}",
            other,
            usage_admin()
        )),
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

async fn remove_device(client: &Client, config: &Config, device_id: &OwnedDeviceId) -> Result<()> {
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

    #[tokio::test]
    async fn test_not_a_command() {
        let config = test_config(vec![]);
        let client = Client::builder()
            .homeserver_url("https://matrix.example.com")
            .build()
            .await
            .unwrap();

        let result = handle_command("hello world", "@user:example.com", &config, &client).await;
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

        let result =
            handle_command("https://example.com", "@user:example.com", &config, &client).await;
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

        let result = handle_command("!embedbot", "@user:example.com", &config, &client).await;
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

        let result =
            handle_command("!embedbot foobar", "@user:example.com", &config, &client).await;
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

        let result = handle_command(
            "!embedbot admin remove-device FOOBAR",
            "@random:example.com",
            &config,
            &client,
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

        let result =
            handle_command("!embedbot admin", "@admin:example.com", &config, &client).await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("Usage")),
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

        let result = handle_command(
            "!embedbot admin nope",
            "@admin:example.com",
            &config,
            &client,
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

        let result = handle_command(
            "!embedbot admin remove-device",
            "@admin:example.com",
            &config,
            &client,
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

        let result = handle_command(
            "!embedbot admin reset-identity",
            "@random:example.com",
            &config,
            &client,
        )
        .await;
        match result {
            CommandResult::Response(msg) => assert!(msg.contains("Permission denied")),
            _ => panic!("Expected Response"),
        }
    }
}

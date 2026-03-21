use std::iter;

use anyhow::{Context, Result};
use matrix_sdk::Client;
use matrix_sdk::ruma::{
    RoomId, UserId,
    api::client::{
        keys::get_keys, to_device::send_event_to_device::v3::Request as RumaToDeviceRequest,
    },
};
use matrix_sdk_base::crypto::{
    CollectStrategy, encrypt_room_key_export, types::events::room_key_bundle::RoomKeyBundleContent,
};
use tracing::{info, warn};

/// Share this device's E2EE room key history for `room_id` with `user_id`.
pub async fn share_room_history(client: &Client, room_id: &RoomId, user_id: &UserId) -> Result<()> {
    let own_identity = match client.user_id() {
        Some(own_user) => client
            .encryption()
            .get_user_identity(own_user)
            .await
            .context("Failed to query own user identity")?,
        None => None,
    };

    if own_identity.is_none() {
        warn!(
            %room_id, %user_id,
            "Not sharing room history: cross-signing is not set up"
        );
        return Ok(());
    }

    info!(%room_id, %user_id, "Sharing room key history");

    let bundle = {
        let guard = client.olm_machine_for_testing().await;
        let olm = guard.as_ref().context("Olm machine is not available")?;
        olm.store()
            .build_room_key_bundle(room_id)
            .await
            .context("Failed to build room key bundle")?
    };

    if bundle.is_empty() {
        info!(%room_id, "No shareable keys for this room");
        return Ok(());
    }

    let shared_count = bundle.room_keys.len();
    let withheld_count = bundle.withheld.len();

    let json = serde_json::to_vec(&bundle).context("Failed to serialise room key bundle")?;
    let encrypted_file = client
        .upload_encrypted_file(&mut json.as_slice())
        .await
        .context("Failed to upload encrypted key bundle")?;

    info!(
        %room_id,
        shared_keys = shared_count,
        withheld_keys = withheld_count,
        "Uploaded encrypted key bundle"
    );

    let keys_query_info = {
        let guard = client.olm_machine_for_testing().await;
        let olm = guard.as_ref().context("Olm machine is not available")?;
        let (req_id, request) = olm.query_keys_for_users(iter::once(user_id));
        if request.device_keys.is_empty() {
            None
        } else {
            Some((req_id, request.device_keys))
        }
    };

    if let Some((req_id, device_keys)) = keys_query_info {
        let mut ruma_request = get_keys::v3::Request::new();
        ruma_request.device_keys = device_keys;

        let response: get_keys::v3::Response = client
            .send(ruma_request)
            .await
            .context("Keys-query request failed")?;

        let guard = client.olm_machine_for_testing().await;
        let olm = guard.as_ref().context("Olm machine is not available")?;
        olm.mark_request_as_sent(&req_id, &response)
            .await
            .context("Failed to process keys-query response")?;
    }

    let claim = {
        let guard = client.olm_machine_for_testing().await;
        let olm = guard.as_ref().context("Olm machine is not available")?;
        olm.get_missing_sessions(iter::once(user_id))
            .await
            .context("Failed to determine missing Olm sessions")?
    };

    if let Some((req_id, request)) = claim {
        let response = client
            .send(request)
            .await
            .context("One-time-key claim request failed")?;

        let guard = client.olm_machine_for_testing().await;
        let olm = guard.as_ref().context("Olm machine is not available")?;
        olm.mark_request_as_sent(&req_id, &response)
            .await
            .context("Failed to process key-claim response")?;
    }

    let content = RoomKeyBundleContent {
        room_id: room_id.to_owned(),
        file: encrypted_file,
    };

    let to_device_requests = {
        let guard = client.olm_machine_for_testing().await;
        let olm = guard.as_ref().context("Olm machine is not available")?;
        olm.share_room_key_bundle_data(user_id, &CollectStrategy::default(), content)
            .await
            .context("Failed to encrypt room key bundle for recipient")?
    };

    for request in to_device_requests {
        let ruma_request = RumaToDeviceRequest::new_raw(
            request.event_type.clone(),
            request.txn_id.clone(),
            request.messages.clone(),
        );

        let response = client
            .send(ruma_request)
            .await
            .context("Failed to send to-device message")?;

        let guard = client.olm_machine_for_testing().await;
        let olm = guard.as_ref().context("Olm machine is not available")?;
        olm.mark_request_as_sent(&request.txn_id, &response)
            .await
            .context("Failed to mark to-device request as sent")?;
    }

    info!(
        %room_id, %user_id,
        shared_keys = shared_count,
        withheld_keys = withheld_count,
        "Room key history shared successfully"
    );

    Ok(())
}

/// Result of a successful room key export.
pub struct KeyExport {
    /// The passphrase used to encrypt the export file.
    pub passphrase: String,
    /// The encrypted export data (Element-compatible format).
    pub data: Vec<u8>,
    /// Number of keys included in the export.
    pub key_count: usize,
}

/// Export this device's room keys for `room_id` in the standard Matrix
/// encrypted key-export format (the same format Element uses for
/// "Export E2E room keys").
///
/// A random passphrase is generated for the export.  The caller is
/// responsible for delivering both the file and the passphrase to the
/// requesting user.
///
/// Returns `Ok(None)` if there are no exportable keys for the room.
pub async fn export_room_keys(client: &Client, room_id: &RoomId) -> Result<Option<KeyExport>> {
    let owned_room_id = room_id.to_owned();

    // 1. Export raw key material from the crypto store.
    let keys = {
        let guard = client.olm_machine_for_testing().await;
        let olm = guard.as_ref().context("Olm machine is not available")?;
        olm.store()
            .export_room_keys(|session| session.room_id() == owned_room_id)
            .await
            .context("Failed to export room keys")?
    };

    if keys.is_empty() {
        return Ok(None);
    }

    let key_count = keys.len();

    // 2. Generate a random passphrase.
    let passphrase = matrix_sdk::ruma::TransactionId::new().to_string();

    // 3. Encrypt into the standard Matrix key-export format.
    let data = {
        let passphrase_clone = passphrase.clone();
        tokio::task::spawn_blocking(move || {
            encrypt_room_key_export(&keys, &passphrase_clone, 500_000)
                .context("Failed to encrypt room key export")
        })
        .await
        .context("Key-export encryption task panicked")??
        .into_bytes()
    };

    info!(
        %room_id,
        key_count,
        export_size = data.len(),
        "Exported room keys"
    );

    Ok(Some(KeyExport {
        passphrase,
        data,
        key_count,
    }))
}

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use matrix_sdk::{
    Client,
    attachment::AttachmentConfig,
    room::{
        Room,
        reply::{EnforceThread, Reply},
    },
    ruma::{
        OwnedEventId,
        events::{
            relation::InReplyTo,
            room::{
                message::{
                    AddMentions, ForwardThread, MessageType, OriginalSyncRoomMessageEvent,
                    Relation, RoomMessageEventContent, TextMessageEventContent,
                },
                redaction::SyncRoomRedactionEvent,
            },
        },
    },
};
use tracing::{debug, error, info, warn};
use url::Url;

use crate::{
    activitypub::ActivityPubDetector,
    cas::MediaStore,
    command,
    config::Config,
    db::{CannedResponse, Database},
    extract::extract_url,
    metadata::Metadata,
    processing::{MessageParams, process_metadata, process_response},
    tracker::{EventTracker, TrackedEntry},
};

/// Determines how the bot's reply relates back to the original message.
enum ReplyTarget {
    Event(Box<OriginalSyncRoomMessageEvent>),
    EventId(OwnedEventId),
}

impl ReplyTarget {
    fn event_id(&self) -> &matrix_sdk::ruma::EventId {
        match self {
            ReplyTarget::Event(ev) => &ev.event_id,
            ReplyTarget::EventId(id) => id,
        }
    }
}

/// Handle an incoming room message event.
///
/// If the event carries an `m.replace` relation it is dispatched to
/// [`handle_replacement`]; otherwise the normal command / URL flow runs.
pub async fn handle_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
    config: Arc<Config>,
    http_client: reqwest::Client,
    client: Client,
    tracker: Arc<EventTracker>,
    ap_detector: Arc<ActivityPubDetector>,
    database: Arc<Database>,
    media_store: Arc<MediaStore>,
) -> Result<()> {
    if let Some(Relation::Replacement(replacement)) = &event.content.relates_to {
        let original_event_id = replacement.event_id.clone();
        let new_msgtype = replacement.new_content.msgtype.clone();
        return handle_replacement(
            original_event_id,
            &new_msgtype,
            room,
            config,
            http_client,
            tracker,
            ap_detector,
        )
        .await;
    }

    match command::handle_command(
        event.content.body(),
        event.sender.as_str(),
        room.room_id().as_str(),
        &config,
        &client,
        &database,
        &http_client,
        &media_store,
        &ap_detector,
    )
    .await
    {
        command::CommandResult::Response(response) => {
            room.send(
                RoomMessageEventContent::text_markdown(response).make_reply_to(
                    &event,
                    ForwardThread::Yes,
                    AddMentions::No,
                ),
            )
            .await?;
            return Ok(());
        }
        command::CommandResult::KeyExport {
            passphrase,
            data,
            key_count,
        } => {
            let caption_text = format!(
                "Here are {} room key(s) for this room.\n\n\
                 **Passphrase:** `{}`\n\n\
                 Import this file in Element via *All Settings → Encryption → Import Keys*. You may need to exit and re-open Element to see old messages.",
                key_count, passphrase,
            );
            let caption = TextMessageEventContent::markdown(caption_text);
            let reply = Reply {
                event_id: event.event_id.clone(),
                enforce_thread: EnforceThread::MaybeThreaded,
            };
            let attach_config = AttachmentConfig::new()
                .caption(Some(caption))
                .reply(Some(reply));

            room.send_attachment(
                "room-keys.txt",
                &"text/plain".parse().expect("valid mime"),
                data,
                attach_config,
            )
            .await?;
            return Ok(());
        }
        command::CommandResult::CannedResponse(canned) => {
            let eid = event.event_id.clone();
            if let Err(e) = send_canned_response(&room, &canned, &eid, &media_store).await {
                error!("Failed to send canned response: {:?}", e);
            }
            return Ok(());
        }
        command::CommandResult::NotACommand => {}
    }

    let url = if let MessageType::Text(text) = &event.content.msgtype {
        extract_url(text, &config)
    } else {
        None
    };

    let body = event.content.body().to_owned();
    let event_id_for_auto = event.event_id.clone();
    let room_id_str = room.room_id().to_string();
    let room_for_auto = room.clone();

    let original_event_id = event.event_id.clone();
    run_embed_task(
        tracker,
        original_event_id,
        ReplyTarget::Event(Box::new(event)),
        room,
        config,
        http_client,
        url,
        ap_detector,
    )
    .await;

    // Autoresponders run last; skipped when earlier branches return early.
    if let Some(canned) = command::check_autoresponders(&body, &room_id_str, &database).await {
        if let Err(e) =
            send_canned_response(&room_for_auto, &canned, &event_id_for_auto, &media_store).await
        {
            warn!("Failed to send autoresponder: {:?}", e);
        }
    }

    Ok(())
}

/// Handle an incoming redaction event.
pub async fn handle_redaction(
    event: SyncRoomRedactionEvent,
    room: Room,
    tracker: Arc<EventTracker>,
) -> Result<()> {
    let SyncRoomRedactionEvent::Original(event) = event else {
        return Ok(());
    };

    // Ignore our own redactions (e.g. self-correction after late cancel).
    if event.sender == room.own_user_id() {
        return Ok(());
    }

    // The target event ID lives at the top level (room versions <= 10) or
    // inside `content` (room versions >= 11).
    let redacted_event_id = event
        .redacts
        .as_ref()
        .or(event.content.redacts.as_ref())
        .cloned();

    let Some(redacted_event_id) = redacted_event_id else {
        warn!("Redaction event {} has no target event ID", event.event_id);
        return Ok(());
    };

    debug!("Processing redaction of event {}", redacted_event_id);

    match tracker.get_event_entry(&redacted_event_id).await {
        Some(TrackedEntry {
            reply_event_id: Some(reply_event_id),
            ..
        }) => {
            info!(
                "Redacting our reply {} (original {} was redacted)",
                reply_event_id, redacted_event_id
            );
            if let Err(e) = room
                .redact(&reply_event_id, Some("Original message was redacted"), None)
                .await
            {
                error!("Failed to redact our reply {}: {:?}", reply_event_id, e);
            }
        }
        _ => {
            debug!(
                "Ignoring redaction for untracked/old event {}",
                redacted_event_id
            );
        }
    }

    Ok(())
}

/// Handle a replacement (edit) of a previously-seen message.
async fn handle_replacement(
    original_event_id: OwnedEventId,
    new_msgtype: &MessageType,
    room: Room,
    config: Arc<Config>,
    http_client: reqwest::Client,
    tracker: Arc<EventTracker>,
    ap_detector: Arc<ActivityPubDetector>,
) -> Result<()> {
    let new_url = if let MessageType::Text(text) = new_msgtype {
        extract_url(text, &config)
    } else {
        None
    };

    debug!(
        "Processing replacement for {}: new_url={:?}",
        original_event_id,
        new_url.as_ref().map(|u| u.as_str())
    );

    match tracker.get_event_entry(&original_event_id).await {
        // Already processed message
        Some(TrackedEntry {
            reply_event_id,
            extracted_url: old_url,
            ..
        }) => {
            if new_url == old_url {
                return Ok(());
            }

            if let Some(reply_event_id) = reply_event_id {
                // There was already a reply; delete it.
                info!(
                    "Redacting outdated reply {} for edited event {}",
                    reply_event_id, original_event_id
                );
                if let Err(e) = room
                    .redact(&reply_event_id, Some("Original message was edited"), None)
                    .await
                {
                    error!("Failed to redact reply {}: {:?}", reply_event_id, e);
                }
            }

            run_embed_task(
                tracker,
                original_event_id.clone(),
                ReplyTarget::EventId(original_event_id),
                room,
                config,
                http_client,
                new_url,
                ap_detector,
            )
            .await;
        }

        // We're not tracking this message, let's ignore it.
        None => {
            debug!(
                "Ignoring replacement of event we don't know about or is too old: {}",
                original_event_id
            );
        }
    }

    Ok(())
}

async fn run_embed_task(
    tracker: Arc<EventTracker>,
    original_event_id: OwnedEventId,
    reply_target: ReplyTarget,
    room: Room,
    config: Arc<Config>,
    http_client: reqwest::Client,
    url: Option<Url>,
    ap_detector: Arc<ActivityPubDetector>,
) {
    match url {
        Some(url) => {
            debug!("Found URL: {}", url);
            match process_and_post(
                &http_client,
                &room,
                &config,
                &url,
                reply_target,
                &ap_detector,
            )
            .await
            {
                Ok(reply_event_id) => {
                    tracker
                        .register(original_event_id, Some(url.clone()), reply_event_id)
                        .await
                }
                Err(e) => {
                    warn!("Failed to process URL {}: {:?}", url, e);
                }
            }
        }
        None => tracker.register(original_event_id, None, None).await,
    }
}

async fn process_and_post(
    http_client: &reqwest::Client,
    room: &Room,
    config: &Config,
    url: &Url,
    reply_target: ReplyTarget,
    ap_detector: &ActivityPubDetector,
) -> Result<Option<OwnedEventId>> {
    let meta = Metadata::fetch_from_url(http_client, url, ap_detector).await?;

    if meta.is_empty() {
        return Ok(None);
    }

    let params = process_metadata(meta, config);

    post_message(http_client, room, config, params, &reply_target, url).await
}

/// Post the embed reply (media and/or text) and return the event ID of
/// the message we sent (if any).
async fn post_message(
    http_client: &reqwest::Client,
    room: &Room,
    config: &Config,
    params: MessageParams,
    reply_target: &ReplyTarget,
    referer: &Url,
) -> Result<Option<OwnedEventId>> {
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

        let reply = Reply {
            event_id: reply_target.event_id().to_owned(),
            enforce_thread: EnforceThread::MaybeThreaded,
        };

        let result = with_typing(
            room,
            download_and_upload(
                http_client,
                room,
                &media_url,
                config,
                caption,
                Some(referer),
                reply,
            ),
        )
        .await;

        match result {
            Ok(event_id) => return Ok(Some(event_id)),
            Err(e) => {
                error!("Failed to upload media: {:?}", e);
                // Fallback: post text embed if available.
                if has_text {
                    let content = make_text_reply(params.body, params.html_body, reply_target);
                    let response = room.send(content).await?;
                    return Ok(Some(response.event_id));
                }
            }
        }
    } else if has_text {
        let content = make_text_reply(params.body, params.html_body, reply_target);
        let response = room.send(content).await?;
        return Ok(Some(response.event_id));
    }

    Ok(None)
}

/// Construct a text reply using the full reply fallback when the
/// original event is available, or a bare `m.in_reply_to` relation
/// otherwise.
fn make_text_reply(
    body: String,
    html_body: String,
    reply_target: &ReplyTarget,
) -> RoomMessageEventContent {
    match reply_target {
        ReplyTarget::Event(event) => RoomMessageEventContent::text_html(body, html_body)
            .make_reply_to(event.as_ref(), ForwardThread::Yes, AddMentions::No),
        ReplyTarget::EventId(id) => {
            let mut content = RoomMessageEventContent::text_html(body, html_body);
            content.relates_to = Some(Relation::Reply {
                in_reply_to: InReplyTo::new(id.clone()),
            });
            content
        }
    }
}

/// Download media from a URL and re-upload it to the Matrix room.
///
/// Returns the event ID of the sent attachment message.
pub async fn download_and_upload(
    client: &reqwest::Client,
    room: &Room,
    url: &Url,
    config: &Config,
    text: Option<TextMessageEventContent>,
    referer: Option<&Url>,
    reply: Reply,
) -> Result<OwnedEventId> {
    let mut request = client.get(url.clone()).timeout(config.download_timeout);
    if let Some(referer) = referer {
        request = request.header(reqwest::header::REFERER, referer.as_str());
    }
    let response = request.send().await.context("Failed to start download")?;

    let attachment = process_response(response, config, text).await?;

    let response = room
        .send_attachment(
            &attachment.filename,
            &attachment.mime_type,
            attachment.data,
            attachment.attachment_config.reply(Some(reply)),
        )
        .await?;

    Ok(response.event_id)
}

/// Send a canned response (from a custom command or autoresponder) as a reply.
async fn send_canned_response(
    room: &Room,
    canned: &CannedResponse,
    event_id: &matrix_sdk::ruma::EventId,
    media_store: &MediaStore,
) -> Result<()> {
    let reply = Reply {
        event_id: event_id.to_owned(),
        enforce_thread: EnforceThread::MaybeThreaded,
    };

    if let Some(cas_hash) = &canned.media_cas_hash {
        let data = media_store.load(cas_hash).await?;
        let mime_str = canned
            .media_mime_type
            .as_deref()
            .unwrap_or("application/octet-stream");
        let mime: mime_guess::Mime = mime_str
            .parse()
            .unwrap_or(mime_guess::mime::APPLICATION_OCTET_STREAM);
        let filename = canned.media_filename.as_deref().unwrap_or("media");
        let caption = canned
            .text_markdown
            .as_deref()
            .map(TextMessageEventContent::markdown);

        let attach_config = AttachmentConfig::new().caption(caption).reply(Some(reply));

        room.send_attachment(filename, &mime, data, attach_config)
            .await?;
    } else if let Some(text) = &canned.text_markdown {
        let mut content = RoomMessageEventContent::text_markdown(text);
        content.relates_to = Some(Relation::Reply {
            in_reply_to: InReplyTo::new(event_id.to_owned()),
        });
        room.send(content).await?;
    }

    Ok(())
}

/// Keep a typing indicator active for the duration of an async operation.
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

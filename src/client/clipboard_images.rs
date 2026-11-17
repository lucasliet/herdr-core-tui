use std::path::PathBuf;

use tracing::{info, warn};

#[cfg(windows)]
use crate::protocol::ClientInputEvent;
use crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD;
use crate::protocol::{ClientClipboardImageTarget, ClientMessage};

use super::{write_to_server, ClientError};

pub(super) fn write_remote_image_with_notice(
    stream: &mut impl super::ClientMessageSink,
    target: ClientClipboardImageTarget,
    image: crate::platform::ClipboardImage,
    source: &'static str,
) -> Result<Option<String>, ClientError> {
    if image.bytes.len() > MAX_CLIPBOARD_IMAGE_PAYLOAD {
        let msg = format!(
            "Image too large to bridge ({} bytes; limit is {} bytes)",
            image.bytes.len(),
            MAX_CLIPBOARD_IMAGE_PAYLOAD
        );
        warn!(
            bytes = image.bytes.len(),
            max = MAX_CLIPBOARD_IMAGE_PAYLOAD,
            source,
            "local image is too large to bridge"
        );
        return Ok(Some(msg));
    }
    write_remote_image_to_server(stream, target, image, source)?;
    Ok(None)
}

pub(super) fn write_remote_image_to_server(
    stream: &mut impl super::ClientMessageSink,
    target: ClientClipboardImageTarget,
    image: crate::platform::ClipboardImage,
    source: &'static str,
) -> Result<(), ClientError> {
    if image.bytes.len() > MAX_CLIPBOARD_IMAGE_PAYLOAD {
        warn!(
            bytes = image.bytes.len(),
            max = MAX_CLIPBOARD_IMAGE_PAYLOAD,
            source,
            "local image is too large to bridge"
        );
        return Ok(());
    }

    info!(
        bytes = image.bytes.len(),
        extension = image.extension,
        source,
        "bridging local image to remote server"
    );
    write_to_server(
        stream,
        &ClientMessage::ClipboardImage {
            target,
            extension: image.extension.to_owned(),
            data: image.bytes,
        },
    )
    .map_err(ClientError::ConnectionLost)
}

pub(super) fn client_remote_image_paste_key(
    config: &crate::config::Config,
) -> Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)> {
    match config.remote_image_paste_key() {
        Ok(key) => key,
        Err(diagnostic) => {
            warn!(diagnostic = %diagnostic, "local remote image paste key config diagnostic");
            None
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn endpoint_accepts_local_images(
    remote_client_process: bool,
    endpoint_id: &super::endpoint::ClientEndpointId,
    active_surface_available: bool,
) -> bool {
    active_surface_available && (remote_client_process || !endpoint_id.is_local())
}

pub(super) fn client_image_bridge_active(
    remote_client_process: bool,
    ssh_session: bool,
    endpoint_id: &super::endpoint::ClientEndpointId,
    active_surface_available: bool,
    focused_remote_machine: bool,
) -> bool {
    active_surface_available
        && (remote_client_process
            || ssh_session
            || focused_remote_machine
            || !endpoint_id.is_local())
}

#[cfg(unix)]
pub(super) fn should_bridge_clipboard_image_paste(
    data: &[u8],
    is_remote_client: bool,
    remote_image_paste_key: Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)>,
) -> bool {
    if !is_remote_client {
        return false;
    }
    if data == b"\x1b[200~\x1b[201~" {
        return true;
    }

    let Some(remote_image_paste_key) = remote_image_paste_key else {
        return false;
    };

    let events = crate::raw_input::parse_framed_input(data);
    matches!(
        events.as_slice(),
        [crate::raw_input::RawInputEvent::Key(key)]
            if key.kind == crossterm::event::KeyEventKind::Press
                && crate::config::terminal_key_matches_combo(key, remote_image_paste_key)
    )
}

#[cfg(windows)]
pub(super) fn should_bridge_clipboard_image_events(
    events: &[ClientInputEvent],
    is_remote_client: bool,
    remote_image_paste_key: Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)>,
) -> bool {
    if !is_remote_client {
        return false;
    }
    if matches!(events, [ClientInputEvent::Paste { text }] if text.is_empty()) {
        return true;
    }

    let Some(remote_image_paste_key) = remote_image_paste_key else {
        return false;
    };
    matches!(
        events,
        [event]
            if matches!(
                event.to_raw_input_event(),
                crate::raw_input::RawInputEvent::Key(key)
                    if key.kind == crossterm::event::KeyEventKind::Press
                        && crate::config::terminal_key_matches_combo(
                            &key,
                            remote_image_paste_key,
                        )
            )
    )
}

#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn read_image_file_from_terminal_drop(
    data: &[u8],
    is_remote_client: bool,
) -> Option<crate::platform::ClipboardImage> {
    read_image_file_drop_result(data, is_remote_client)
        .ok()
        .flatten()
}

#[cfg(unix)]
pub(super) fn read_image_file_drop_result(
    data: &[u8],
    is_remote_client: bool,
) -> Result<Option<crate::platform::ClipboardImage>, String> {
    let Some((path, extension)) = image_path_from_terminal_drop(data, is_remote_client) else {
        return Ok(None);
    };
    read_image_file_result(path, extension)
}

#[cfg(windows)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn read_image_file_from_client_events(
    events: &[ClientInputEvent],
    is_remote_client: bool,
) -> Option<crate::platform::ClipboardImage> {
    read_image_file_events_result(events, is_remote_client)
        .ok()
        .flatten()
}

#[cfg(windows)]
pub(super) fn read_image_file_events_result(
    events: &[ClientInputEvent],
    is_remote_client: bool,
) -> Result<Option<crate::platform::ClipboardImage>, String> {
    let [ClientInputEvent::Paste { text }] = events else {
        return Ok(None);
    };
    let Some(text) = normalized_terminal_drop_text(text) else {
        return Ok(None);
    };
    let Some((path, extension)) =
        image_path_from_drop_text(strip_matching_path_quotes(text), is_remote_client)
    else {
        return Ok(None);
    };
    read_image_file_result(path, extension)
}

fn read_image_file_result(
    path: PathBuf,
    extension: &'static str,
) -> Result<Option<crate::platform::ClipboardImage>, String> {
    let Ok(metadata) = std::fs::metadata(&path) else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Ok(None);
    }

    let file = std::fs::File::open(&path)
        .map_err(|err| format!("Failed to read dropped image {}: {err}", path.display()))?;
    let bytes = match crate::platform::read_limited_reader(file, MAX_CLIPBOARD_IMAGE_PAYLOAD) {
        Ok(crate::platform::LimitedRead::Complete(bytes)) => bytes,
        Ok(crate::platform::LimitedRead::Empty) => {
            return Err(format!("Dropped image is empty: {}", path.display()));
        }
        Ok(crate::platform::LimitedRead::Oversized) => {
            warn!(
                max = MAX_CLIPBOARD_IMAGE_PAYLOAD,
                "local image file drop is too large to bridge"
            );
            return Err(format!(
                "Dropped image exceeds the {} byte bridge limit",
                MAX_CLIPBOARD_IMAGE_PAYLOAD
            ));
        }
        Err(err) => {
            return Err(format!(
                "Failed to read dropped image {}: {err}",
                path.display()
            ));
        }
    };

    Ok(Some(crate::platform::ClipboardImage { bytes, extension }))
}

#[cfg(unix)]
pub(super) fn image_path_from_terminal_drop(
    data: &[u8],
    is_remote_client: bool,
) -> Option<(PathBuf, &'static str)> {
    let bytes = bracketed_paste_payload(data).unwrap_or(data);
    let text = std::str::from_utf8(bytes).ok()?;
    let text = normalized_terminal_drop_text(text)?;
    let text = unescape_terminal_drop_path(strip_matching_path_quotes(text));
    image_path_from_drop_text(&text, is_remote_client)
}

fn normalized_terminal_drop_text(text: &str) -> Option<&str> {
    let text = text.trim_end_matches(['\r', '\n']);
    (!text.is_empty() && !text.contains(['\r', '\n'])).then_some(text)
}

fn image_path_from_drop_text(
    text: &str,
    is_remote_client: bool,
) -> Option<(PathBuf, &'static str)> {
    if !is_remote_client {
        return None;
    }
    let path = PathBuf::from(text);
    if !path.is_absolute() {
        return None;
    }
    let extension = recognized_image_extension(path.extension()?.to_str()?)?;
    Some((path, extension))
}

#[cfg(unix)]
fn bracketed_paste_payload(data: &[u8]) -> Option<&[u8]> {
    const START: &[u8] = b"\x1b[200~";
    const END: &[u8] = b"\x1b[201~";
    data.strip_prefix(START)?.strip_suffix(END)
}

fn strip_matching_path_quotes(text: &str) -> &str {
    if text.len() < 2 {
        return text;
    }

    let bytes = text.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(b'\''), Some(b'\'')) | (Some(b'"'), Some(b'"')) => &text[1..text.len() - 1],
        _ => text,
    }
}

#[cfg(unix)]
fn unescape_terminal_drop_path(text: &str) -> String {
    let mut unescaped = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(escaped) = chars.next() {
                unescaped.push(escaped);
            } else {
                unescaped.push(ch);
            }
        } else {
            unescaped.push(ch);
        }
    }
    unescaped
}

fn recognized_image_extension(extension: &str) -> Option<&'static str> {
    if extension.eq_ignore_ascii_case("png") {
        Some("png")
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        Some("jpg")
    } else if extension.eq_ignore_ascii_case("gif") {
        Some("gif")
    } else if extension.eq_ignore_ascii_case("webp") {
        Some("webp")
    } else if extension.eq_ignore_ascii_case("bmp") {
        Some("bmp")
    } else {
        None
    }
}

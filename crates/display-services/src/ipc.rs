//! The typed operations the two services exchange.
//!
//! One datagram is one message: the socket is `SOCK_SEQPACKET`, so nothing here
//! frames anything. Descriptors ride alongside as `SCM_RIGHTS` and are named by
//! [`Message::Snapshot`]'s `new_buffers` rather than by their position in the
//! payload, so that a peer which already holds a buffer is not sent it again.

use std::{error::Error, fmt};

use prost::Message as _;
use vmlord_display_codec::Rect;

use crate::broker::{self, envelope};

/// What one side asks the other to do, or tells it has happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// The unprivileged process introducing itself.
    Attach,
    /// It is ready for another frame.
    NextFrame,
    /// A control handshake completed.
    SessionOpened(SessionParameters),
    /// A control handshake completed, as the clipboard daemon needs it.
    ClipboardOpened {
        /// The 16 bytes that name the session across its four sockets.
        session_id: Vec<u8>,
        /// The key the clipboard socket proves itself with.
        clipboard_key: Vec<u8>,
    },
    /// Control was lost, or the host is finished.
    SessionClosed {
        /// What to put in the journal. Never parsed.
        reason: String,
    },
    /// The viewer needs a whole frame.
    KeyframeRequested,
    /// The guest's keyboard and pointer, whose descriptors are attached to the
    /// datagram in that order.
    InputDevices,
    /// The output is now this big, read off the framebuffer rather than off
    /// what was asked for.
    Geometry {
        /// Its width in pixels.
        width: u32,
        /// Its height.
        height: u32,
        /// The refresh the compositor committed, or zero when the CRTC would
        /// not say what it is scanning out.
        refresh_hz: u32,
    },
    /// What the planes hold at one vblank.
    Snapshot {
        /// The vblank this was taken at.
        sequence: u64,
        /// The planes that had a framebuffer.
        planes: Vec<PlaneLayout>,
        /// The buffers whose descriptors are attached, in that order.
        new_buffers: Vec<u64>,
    },
    /// Something the host should be told about.
    Report {
        /// What to put in the `Error` record's detail.
        detail: String,
    },
}

/// What a frame and an input channel need, and nothing more.
///
/// The secret is not here and never crosses this socket: what a compromised
/// capture process could take from these bytes is one session, and only while
/// that session runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionParameters {
    /// The 16 bytes that name the session across its three sockets.
    pub session_id: Vec<u8>,
    /// The key the frame socket proves itself with.
    pub frame_key: Vec<u8>,
    /// The key the input socket proves itself with.
    pub input_key: Vec<u8>,
    /// The width the session displays.
    pub width: u32,
    /// The height the session displays.
    pub height: u32,
    /// The tile size the frame stream uses for the life of the session.
    pub tile_size: u32,
    /// Whether the cursor travels as its own records rather than in the frame.
    pub cursor_stream: bool,
}

/// Which plane a layout describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneKind {
    /// What the desktop is drawn into.
    Primary,
    /// Where mutter puts the pointer, which is why capture composites.
    Cursor,
}

/// One plane at one vblank.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaneLayout {
    /// Which plane this is.
    pub kind: PlaneKind,
    /// The framebuffer id, which is also what names the descriptor.
    pub buffer: u64,
    /// The framebuffer's width in pixels.
    pub width: u32,
    /// The framebuffer's height in pixels.
    pub height: u32,
    /// Bytes per row, which is not promised to be `width * 4`.
    pub stride: u32,
    /// The DRM fourcc, checked against what this build will map.
    pub format: u32,
    /// The plane's left edge on the CRTC. Negative at the left edge.
    pub x: i32,
    /// The plane's top edge on the CRTC. Negative at the top edge.
    pub y: i32,
    /// What this commit repainted, when that is known in full.
    ///
    /// `None` is "unknown", and the encoder answers it by comparing the frame
    /// against its reference. `Some` of an empty list is the other answer: a
    /// commit that repainted nothing at all.
    pub damage: Option<Vec<Rect>>,
}

/// A datagram this build cannot read.
#[derive(Debug)]
pub struct IpcError(String);

impl fmt::Display for IpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "the display broker socket carried {}", self.0)
    }
}

impl Error for IpcError {}

/// Writes a message as one datagram's payload.
#[must_use]
pub fn encode(message: &Message) -> Vec<u8> {
    broker::Envelope {
        message: Some(into_wire(message)),
    }
    .encode_to_vec()
}

/// Reads one datagram's payload.
///
/// # Errors
///
/// [`IpcError`] for bytes that are not an `Envelope`, for an envelope with no
/// arm, or for a plane that names no kind this build has.
pub fn decode(bytes: &[u8]) -> Result<Message, IpcError> {
    let envelope = broker::Envelope::decode(bytes)
        .map_err(|error| IpcError(format!("bytes that are not an envelope: {error}")))?;
    let Some(message) = envelope.message else {
        return Err(IpcError("an envelope with no message".to_owned()));
    };

    from_wire(message)
}

fn into_wire(message: &Message) -> envelope::Message {
    match message {
        Message::Attach => envelope::Message::Attach(broker::Attach {}),
        Message::NextFrame => envelope::Message::NextFrame(broker::NextFrame {}),
        Message::SessionOpened(parameters) => {
            envelope::Message::SessionOpened(broker::SessionOpened {
                session_id: parameters.session_id.clone(),
                frame_key: parameters.frame_key.clone(),
                input_key: parameters.input_key.clone(),
                width: parameters.width,
                height: parameters.height,
                tile_size: parameters.tile_size,
                cursor_stream: parameters.cursor_stream,
            })
        }
        Message::ClipboardOpened {
            session_id,
            clipboard_key,
        } => envelope::Message::ClipboardOpened(broker::ClipboardOpened {
            session_id: session_id.clone(),
            clipboard_key: clipboard_key.clone(),
        }),
        Message::SessionClosed { reason } => {
            envelope::Message::SessionClosed(broker::SessionClosed {
                reason: reason.clone(),
            })
        }
        Message::KeyframeRequested => {
            envelope::Message::KeyframeRequested(broker::KeyframeRequested {})
        }
        Message::InputDevices => envelope::Message::InputDevices(broker::InputDevices {}),
        Message::Geometry {
            width,
            height,
            refresh_hz,
        } => envelope::Message::Geometry(broker::Geometry {
            width: *width,
            height: *height,
            refresh_hz: *refresh_hz,
        }),
        Message::Snapshot {
            sequence,
            planes,
            new_buffers,
        } => envelope::Message::Snapshot(broker::Snapshot {
            sequence: *sequence,
            planes: planes.iter().map(plane_into_wire).collect(),
            new_buffers: new_buffers.clone(),
        }),
        Message::Report { detail } => envelope::Message::Report(broker::Report {
            detail: detail.clone(),
        }),
    }
}

fn from_wire(message: envelope::Message) -> Result<Message, IpcError> {
    Ok(match message {
        envelope::Message::Attach(_) => Message::Attach,
        envelope::Message::NextFrame(_) => Message::NextFrame,
        envelope::Message::SessionOpened(opened) => Message::SessionOpened(SessionParameters {
            session_id: opened.session_id,
            frame_key: opened.frame_key,
            input_key: opened.input_key,
            width: opened.width,
            height: opened.height,
            tile_size: opened.tile_size,
            cursor_stream: opened.cursor_stream,
        }),
        envelope::Message::ClipboardOpened(opened) => Message::ClipboardOpened {
            session_id: opened.session_id,
            clipboard_key: opened.clipboard_key,
        },
        envelope::Message::SessionClosed(closed) => Message::SessionClosed {
            reason: closed.reason,
        },
        envelope::Message::KeyframeRequested(_) => Message::KeyframeRequested,
        envelope::Message::InputDevices(_) => Message::InputDevices,
        envelope::Message::Geometry(geometry) => Message::Geometry {
            width: geometry.width,
            height: geometry.height,
            refresh_hz: geometry.refresh_hz,
        },
        envelope::Message::Snapshot(snapshot) => Message::Snapshot {
            sequence: snapshot.sequence,
            planes: snapshot
                .planes
                .iter()
                .map(plane_from_wire)
                .collect::<Result<_, _>>()?,
            new_buffers: snapshot.new_buffers,
        },
        envelope::Message::Report(report) => Message::Report {
            detail: report.detail,
        },
    })
}

fn plane_into_wire(plane: &PlaneLayout) -> broker::PlaneLayout {
    broker::PlaneLayout {
        damage_known: plane.damage.is_some(),
        damage: plane
            .damage
            .iter()
            .flatten()
            .map(|rect| broker::DamageRect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            })
            .collect(),
        kind: i32::from(match plane.kind {
            PlaneKind::Primary => broker::PlaneKind::Primary,
            PlaneKind::Cursor => broker::PlaneKind::Cursor,
        }),
        buffer: plane.buffer,
        width: plane.width,
        height: plane.height,
        stride: plane.stride,
        format: plane.format,
        x: plane.x,
        y: plane.y,
    }
}

fn plane_from_wire(plane: &broker::PlaneLayout) -> Result<PlaneLayout, IpcError> {
    let kind = match plane.kind() {
        broker::PlaneKind::Primary => PlaneKind::Primary,
        broker::PlaneKind::Cursor => PlaneKind::Cursor,
        // Proto3's "absent", not a plane. A layout that names no plane is one
        // nothing here can composite.
        broker::PlaneKind::Unspecified => {
            return Err(IpcError(format!(
                "a layout for buffer {} that names no plane",
                plane.buffer
            )));
        }
    };

    Ok(PlaneLayout {
        kind,
        damage: plane.damage_known.then(|| {
            plane
                .damage
                .iter()
                .map(|rect| Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                })
                .collect()
        }),
        buffer: plane.buffer,
        width: plane.width,
        height: plane.height,
        stride: plane.stride,
        format: plane.format,
        x: plane.x,
        y: plane.y,
    })
}

#[cfg(test)]
mod tests {
    use super::{Message, PlaneKind, PlaneLayout, Rect, SessionParameters, decode, encode};

    fn parameters() -> SessionParameters {
        SessionParameters {
            session_id: vec![7; 16],
            frame_key: vec![1; 32],
            input_key: vec![2; 32],
            width: 1920,
            height: 1080,
            tile_size: 32,
            cursor_stream: true,
        }
    }

    #[test]
    fn every_message_survives_a_round_trip() {
        let messages = [
            Message::Attach,
            Message::NextFrame,
            Message::SessionOpened(parameters()),
            Message::ClipboardOpened {
                session_id: vec![7; 16],
                clipboard_key: vec![9; 32],
            },
            Message::SessionClosed {
                reason: "control was lost".into(),
            },
            Message::KeyframeRequested,
            Message::Geometry {
                width: 2560,
                height: 1440,
                refresh_hz: 144,
            },
            Message::Snapshot {
                sequence: 42,
                planes: vec![PlaneLayout {
                    kind: PlaneKind::Primary,
                    damage: None,
                    buffer: 3,
                    width: 1920,
                    height: 1080,
                    stride: 7680,
                    format: crate::drm::uapi::DRM_FORMAT_XRGB8888,
                    x: -12,
                    y: 0,
                }],
                new_buffers: vec![3],
            },
            Message::Report {
                detail: "capture failed".into(),
            },
        ];

        for message in messages {
            let bytes = encode(&message);
            assert_eq!(decode(&bytes).expect("a message this build wrote"), message);
        }
    }

    #[test]
    fn a_commit_that_repainted_nothing_survives_the_wire_as_itself() {
        // The distinction the whole scheme rests on: an empty list is "nothing
        // changed" and absent is "nobody knows". Proto3 has no optional
        // repeated field, so the flag beside it is what carries the
        // difference, and a round trip that lost it would make an unknown
        // frame look like an unchanged one -- and freeze the viewer's picture.
        for damage in [
            None,
            Some(Vec::new()),
            Some(vec![Rect {
                x: 16,
                y: 32,
                width: 8,
                height: 4,
            }]),
        ] {
            let message = Message::Snapshot {
                sequence: 1,
                planes: vec![PlaneLayout {
                    kind: PlaneKind::Primary,
                    damage: damage.clone(),
                    buffer: 3,
                    width: 64,
                    height: 64,
                    stride: 256,
                    format: crate::drm::uapi::DRM_FORMAT_XRGB8888,
                    x: 0,
                    y: 0,
                }],
                new_buffers: Vec::new(),
            };

            assert_eq!(
                decode(&encode(&message)).expect("this build wrote it"),
                message
            );
        }
    }

    #[test]
    fn a_message_this_build_cannot_name_is_refused() {
        assert!(decode(&[0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn a_layout_that_names_no_plane_is_refused() {
        // Proto3 cannot tell "absent" from the first variant, which is why the
        // schema keeps a zero value and why reading one back is an error rather
        // than a plane this build guesses at.
        use prost::Message as _;

        let envelope = crate::broker::Envelope {
            message: Some(crate::broker::envelope::Message::Snapshot(
                crate::broker::Snapshot {
                    sequence: 1,
                    planes: vec![crate::broker::PlaneLayout {
                        kind: 0,
                        buffer: 4,
                        ..Default::default()
                    }],
                    new_buffers: Vec::new(),
                },
            )),
        };

        assert!(decode(&envelope.encode_to_vec()).is_err());
    }

    #[test]
    fn a_negative_plane_position_survives_the_wire() {
        let encoded = encode(&Message::Snapshot {
            sequence: 1,
            planes: vec![PlaneLayout {
                kind: PlaneKind::Cursor,
                damage: None,
                buffer: 9,
                width: 64,
                height: 64,
                stride: 256,
                format: crate::drm::uapi::DRM_FORMAT_ARGB8888,
                x: -30,
                y: -7,
            }],
            new_buffers: Vec::new(),
        });
        let Message::Snapshot { planes, .. } =
            decode(&encoded).expect("a message this build wrote")
        else {
            panic!("a snapshot decodes as a snapshot");
        };

        assert_eq!((planes[0].x, planes[0].y), (-30, -7));
    }

    #[test]
    fn the_input_devices_message_survives_a_round_trip() {
        let message = Message::InputDevices;

        assert_eq!(decode(&encode(&message)).expect("a message"), message);
    }
}

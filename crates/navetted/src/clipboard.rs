//! The clipboard decision layer.
//!
//! Deliberately free of wprs types, transport and I/O: every decision the
//! feature makes is a plain function of the events it has seen, so the
//! whole state machine is unit-testable and `bridge.rs` is reduced to
//! translation.
//!
//! Clipboard content never appears in a log line here or anywhere else.

use navette_protocol::media::MAX_CLIPBOARD_BYTES;

/// Offered to the guest when the phone sets a clipboard value, and
/// searched in this order when the guest offers one. `UTF8_STRING`,
/// `STRING` and `TEXT` are X11 atoms and arrive from XWayland guests,
/// which is the common case for browsers.
pub const OFFERED_MIME_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

/// Something the guest side did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GuestEvent {
    /// The guest set its selection and offered these MIME types.
    SelectionOffered { mime_types: Vec<String> },
    /// The guest sent the bytes we asked for.
    TransferFromGuest { bytes: Vec<u8> },
    /// A guest application pasted and wants our data.
    PasteRequested,
}

/// What the caller should do next. Exactly one action per event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncAction {
    Nothing,
    AskGuestFor { mime: String },
    PushToPhone { text: String },
    OfferToGuest { mime_types: Vec<String> },
    AnswerGuest { bytes: Vec<u8> },
}

#[derive(Debug, Default)]
pub struct ClipboardSync {
    /// The phone's latest clipboard text, retained to answer a guest paste
    /// that may arrive seconds later, or never.
    phone_text: Option<String>,
    /// Set when we ask the guest for data, cleared when a transfer
    /// consumes it. A transfer arriving with this unset answers no request
    /// we made and is dropped.
    awaiting_guest_transfer: bool,
    /// One-shot echo tokens, each named for the direction the echo arrives
    /// from. Set when we send in that direction; consumed by the first
    /// matching inbound value. They MUST be cleared on match: a retained
    /// token silently suppresses the same text legitimately copied later,
    /// forever.
    echo_from_guest: Option<String>,
    echo_from_phone: Option<String>,
}

impl ClipboardSync {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_guest(&mut self, event: GuestEvent) -> SyncAction {
        match event {
            GuestEvent::SelectionOffered { mime_types } => {
                match select_text_mime(&mime_types) {
                    Some(mime) => {
                        self.awaiting_guest_transfer = true;
                        SyncAction::AskGuestFor { mime }
                    }
                    // The guest offered no text form. That is a real
                    // state, not an empty string.
                    None => SyncAction::Nothing,
                }
            }
            GuestEvent::TransferFromGuest { bytes } => {
                if !self.awaiting_guest_transfer {
                    return SyncAction::Nothing;
                }
                self.awaiting_guest_transfer = false;

                if bytes.len() > MAX_CLIPBOARD_BYTES {
                    tracing::debug!(
                        bytes = bytes.len(),
                        "dropping oversized clipboard transfer from guest"
                    );
                    return SyncAction::Nothing;
                }
                let Ok(text) = String::from_utf8(bytes) else {
                    tracing::debug!("dropping non-UTF-8 clipboard transfer from guest");
                    return SyncAction::Nothing;
                };

                if self.echo_from_guest.as_deref() == Some(text.as_str()) {
                    self.echo_from_guest = None;
                    return SyncAction::Nothing;
                }

                self.echo_from_phone = Some(text.clone());
                SyncAction::PushToPhone { text }
            }
            // Always answer. wprsd has taken the pipe fd; leaving it
            // unwritten and unclosed hangs the pasting guest app forever.
            GuestEvent::PasteRequested => SyncAction::AnswerGuest {
                bytes: self
                    .phone_text
                    .as_ref()
                    .map(|text| text.as_bytes().to_vec())
                    .unwrap_or_default(),
            },
        }
    }

    pub fn on_phone_clipboard(&mut self, text: String) -> SyncAction {
        if self.echo_from_phone.as_deref() == Some(text.as_str()) {
            self.echo_from_phone = None;
            return SyncAction::Nothing;
        }

        self.echo_from_guest = Some(text.clone());
        self.phone_text = Some(text);
        SyncAction::OfferToGuest {
            mime_types: OFFERED_MIME_TYPES
                .iter()
                .map(|mime| (*mime).to_string())
                .collect(),
        }
    }
}

/// Pick the guest's own spelling of the most preferred text MIME type it
/// offered. Comparison ignores ASCII whitespace and case so that
/// `text/plain; charset=utf-8` matches, but the returned string is the one
/// the guest actually offered — asking with a normalised spelling it never
/// advertised would not match.
fn select_text_mime(offered: &[String]) -> Option<String> {
    OFFERED_MIME_TYPES.iter().find_map(|preferred| {
        let wanted = normalize_mime(preferred);
        offered
            .iter()
            .find(|candidate| normalize_mime(candidate) == wanted)
            .cloned()
    })
}

fn normalize_mime(mime: &str) -> String {
    mime.chars()
        .filter(|character| !character.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(mimes: &[&str]) -> GuestEvent {
        GuestEvent::SelectionOffered {
            mime_types: mimes.iter().map(|m| (*m).to_string()).collect(),
        }
    }

    #[test]
    fn prefers_utf8_plain_text_over_other_spellings() {
        let mut sync = ClipboardSync::new();
        let action = sync.on_guest(offer(&["STRING", "text/plain", "text/plain;charset=utf-8"]));
        assert_eq!(
            action,
            SyncAction::AskGuestFor {
                mime: "text/plain;charset=utf-8".into()
            }
        );
    }

    #[test]
    fn falls_back_through_the_x11_atoms() {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_guest(offer(&["TEXT", "STRING"])),
            SyncAction::AskGuestFor {
                mime: "STRING".into()
            }
        );
    }

    #[test]
    fn asks_using_the_guests_own_spelling() {
        // The guest offered a spaced variant. We must ask for the string it
        // actually offered, not our normalised form, or it will not match.
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_guest(offer(&["text/plain; charset=utf-8"])),
            SyncAction::AskGuestFor {
                mime: "text/plain; charset=utf-8".into()
            }
        );
    }

    #[test]
    fn an_offer_with_no_text_form_is_ignored() {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_guest(offer(&["image/png", "application/pdf"])),
            SyncAction::Nothing
        );
    }

    #[test]
    fn guest_transfer_is_pushed_to_the_phone() {
        let mut sync = ClipboardSync::new();
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"hello".to_vec()
            }),
            SyncAction::PushToPhone {
                text: "hello".into()
            }
        );
    }

    #[test]
    fn invalid_utf8_from_the_guest_is_dropped() {
        let mut sync = ClipboardSync::new();
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: vec![0xff, 0xfe, 0xfd]
            }),
            SyncAction::Nothing
        );
    }

    #[test]
    fn oversized_guest_transfer_is_dropped() {
        let mut sync = ClipboardSync::new();
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: vec![b'a'; MAX_CLIPBOARD_BYTES + 1]
            }),
            SyncAction::Nothing
        );
    }

    #[test]
    fn phone_clipboard_is_offered_to_the_guest() {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_phone_clipboard("hello".into()),
            SyncAction::OfferToGuest {
                mime_types: OFFERED_MIME_TYPES
                    .iter()
                    .map(|m| (*m).to_string())
                    .collect()
            }
        );
    }

    #[test]
    fn a_paste_is_answered_with_the_phones_text() {
        let mut sync = ClipboardSync::new();
        sync.on_phone_clipboard("hello".into());
        assert_eq!(
            sync.on_guest(GuestEvent::PasteRequested),
            SyncAction::AnswerGuest {
                bytes: b"hello".to_vec()
            }
        );
    }

    /// The hang regression. wprsd takes the pipe fd when it forwards the
    /// paste; if we answer with nothing, that pipe is never written and
    /// never closed, and the pasting guest app blocks on read forever.
    #[test]
    fn a_paste_with_no_phone_text_is_still_answered() {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_guest(GuestEvent::PasteRequested),
            SyncAction::AnswerGuest { bytes: Vec::new() }
        );
    }

    #[test]
    fn our_own_value_coming_back_from_the_guest_is_suppressed() {
        let mut sync = ClipboardSync::new();
        sync.on_phone_clipboard("hello".into());
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"hello".to_vec()
            }),
            SyncAction::Nothing,
            "the value we just sent to the guest must not bounce back"
        );
    }

    #[test]
    fn our_own_value_coming_back_from_the_phone_is_suppressed() {
        let mut sync = ClipboardSync::new();
        sync.on_guest(offer(&["text/plain"]));
        sync.on_guest(GuestEvent::TransferFromGuest {
            bytes: b"hello".to_vec(),
        });
        assert_eq!(
            sync.on_phone_clipboard("hello".into()),
            SyncAction::Nothing,
            "the value we just pushed to the phone must not bounce back"
        );
    }

    /// The discriminating test. A token that is merely compared and
    /// retained passes every distinct-string test above and fails only
    /// this one: the same text, legitimately copied again on the other
    /// side, must still propagate.
    #[test]
    fn the_echo_token_is_one_shot() {
        let mut sync = ClipboardSync::new();

        // Phone sends "hello"; the guest echoes it straight back.
        sync.on_phone_clipboard("hello".into());
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"hello".to_vec()
            }),
            SyncAction::Nothing
        );

        // Later, a user genuinely copies "hello" in a guest window again.
        // This must reach the phone.
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"hello".to_vec()
            }),
            SyncAction::PushToPhone {
                text: "hello".into()
            },
            "suppression is one-shot; the same text copied again must propagate"
        );
    }

    #[test]
    fn phone_echo_token_is_also_one_shot() {
        let mut sync = ClipboardSync::new();
        sync.on_guest(offer(&["text/plain"]));
        sync.on_guest(GuestEvent::TransferFromGuest {
            bytes: b"hello".to_vec(),
        });
        assert_eq!(sync.on_phone_clipboard("hello".into()), SyncAction::Nothing);
        assert_eq!(
            sync.on_phone_clipboard("hello".into()),
            SyncAction::OfferToGuest {
                mime_types: OFFERED_MIME_TYPES
                    .iter()
                    .map(|m| (*m).to_string())
                    .collect()
            },
            "the second genuine copy of the same text must propagate"
        );
    }

    /// A transfer arriving with no offer in flight belongs to no request we
    /// made. wprsd keeps one pipe slot per source and overwrites rather
    /// than correlating, so we match that model instead of promising more.
    #[test]
    fn a_transfer_with_no_outstanding_request_is_dropped() {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"hello".to_vec()
            }),
            SyncAction::Nothing
        );
    }

    #[test]
    fn a_re_offer_supersedes_the_previous_request() {
        let mut sync = ClipboardSync::new();
        sync.on_guest(offer(&["text/plain"]));
        sync.on_guest(offer(&["text/plain"]));
        // Exactly one transfer is consumed by the outstanding request.
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"second".to_vec()
            }),
            SyncAction::PushToPhone {
                text: "second".into()
            }
        );
        // A second transfer has no request left to answer.
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"stale".to_vec()
            }),
            SyncAction::Nothing
        );
    }
}

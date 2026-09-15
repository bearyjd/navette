//! The clipboard decision layer.
//!
//! Deliberately free of wprs types, transport and I/O: every decision the
//! feature makes is a plain function of the events it has seen, so the
//! whole state machine is unit-testable and `bridge.rs` is reduced to
//! translation.
//!
//! Clipboard content never appears in a log line here or anywhere else.

use navette_protocol::media::{BlobDescriptor, MAX_CLIPBOARD_BYTES};

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

/// Image formats supported by the bulk clipboard transport.
pub const OFFERED_IMAGE_MIME_TYPES: [&str; 3] = ["image/png", "image/jpeg", "image/webp"];

/// Something the guest side did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GuestEvent {
    /// The guest set its selection and offered these MIME types.
    SelectionOffered { mime_types: Vec<String> },
    /// The guest sent the bytes we asked for.
    TransferFromGuest { bytes: Vec<u8> },
    /// The bridge stored a guest image and gives this pure state machine its
    /// descriptor. The state machine never sees or opens blob paths.
    TransferBlobFromGuest { blob: BlobDescriptor },
    /// The bridge could not store a guest image transfer. This disarms the
    /// pending request so a later stale transfer cannot be attributed to it.
    TransferBlobFailed,
    /// A guest application pasted and wants our data.
    PasteRequested { mime: String },
}

/// What the caller should do next. Exactly one action per event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncAction {
    Nothing,
    AskGuestFor { mime: String },
    PushToPhone { text: String },
    PushBlobToPhone { blob: BlobDescriptor },
    OfferToGuest { mime_types: Vec<String> },
    AnswerGuest { bytes: Vec<u8> },
    AnswerGuestBlob { blob: BlobDescriptor },
}

#[derive(Debug)]
enum PhoneClipboard {
    Text(String),
    Blob(BlobDescriptor),
}

#[derive(Debug, Default)]
pub struct ClipboardSync {
    /// The phone's latest clipboard text, retained to answer a guest paste
    /// that may arrive seconds later, or never.
    phone_clipboard: Option<PhoneClipboard>,
    /// Set when we ask the guest for data, cleared when a transfer
    /// consumes it. A transfer arriving with this unset answers no request
    /// we made and is dropped.
    awaiting_guest_transfer: Option<AwaitingGuestTransfer>,
    pending_guest_mime: Option<String>,
    /// One-shot echo tokens, each named for the direction the echo arrives
    /// from. Set when we send in that direction; consumed by the first
    /// matching inbound value. They MUST be cleared on match: a retained
    /// token silently suppresses the same text legitimately copied later,
    /// forever.
    echo_from_guest: Option<String>,
    echo_from_phone: Option<String>,
    echo_blob_from_guest: Option<String>,
    echo_blob_from_phone: Option<String>,
    /// Blob objects that stopped being the current clipboard value. The
    /// bridge drains this after each transition, keeping I/O out of this
    /// pure decision layer while avoiding an unbounded session blob cache.
    retired_blobs: Vec<BlobDescriptor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AwaitingGuestTransfer {
    Text,
    Blob,
}

impl ClipboardSync {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_guest(&mut self, event: GuestEvent) -> SyncAction {
        match event {
            GuestEvent::SelectionOffered { mime_types } => {
                match select_text_mime(&mime_types).or_else(|| select_image_mime(&mime_types)) {
                    Some(mime) => {
                        self.awaiting_guest_transfer = Some(if is_image_mime(&mime) {
                            AwaitingGuestTransfer::Blob
                        } else {
                            AwaitingGuestTransfer::Text
                        });
                        self.pending_guest_mime = Some(mime.clone());
                        SyncAction::AskGuestFor { mime }
                    }
                    // The guest offered no text form. That is a real
                    // state, not an empty string -- but it still supersedes
                    // any outstanding request the same way a re-offer with
                    // a text form does (see `a_re_offer_supersedes_the_
                    // previous_request` below): a selection that had text
                    // and was replaced by one that does not must not leave
                    // `awaiting_guest_transfer` set. Left set, a transfer
                    // that lands late for the *old*, superseded selection
                    // passes the `!awaiting_guest_transfer` guard below and
                    // overwrites the phone's clipboard from a selection
                    // that, by the time those bytes arrive, has no text
                    // form at all.
                    None => {
                        self.awaiting_guest_transfer = None;
                        self.pending_guest_mime = None;
                        SyncAction::Nothing
                    }
                }
            }
            GuestEvent::TransferFromGuest { bytes } => {
                if self.awaiting_guest_transfer != Some(AwaitingGuestTransfer::Text) {
                    return SyncAction::Nothing;
                }
                self.awaiting_guest_transfer = None;
                self.pending_guest_mime = None;

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

                // Mirrors the Kotlin side's `text.isEmpty()` check in
                // `ClipboardBridge.onLocalClipboard`. Without this, an empty
                // guest transfer sets `phone_text = Some("")` and every
                // later guest paste is answered with empty bytes until the
                // phone genuinely sets something -- a real state, just not
                // one worth propagating.
                if text.is_empty() {
                    return SyncAction::Nothing;
                }

                if self.echo_from_guest.as_deref() == Some(text.as_str()) {
                    self.echo_from_guest = None;
                    return SyncAction::Nothing;
                }

                self.echo_from_phone = Some(text.clone());
                self.replace_phone_clipboard(PhoneClipboard::Text(text.clone()));
                SyncAction::PushToPhone { text }
            }
            GuestEvent::TransferBlobFromGuest { blob } => {
                if self.awaiting_guest_transfer != Some(AwaitingGuestTransfer::Blob) {
                    return SyncAction::Nothing;
                }
                self.awaiting_guest_transfer = None;
                self.pending_guest_mime = None;
                if blob.validate().is_err() {
                    return SyncAction::Nothing;
                }
                if self.echo_blob_from_guest.as_deref() == Some(blob.id.as_str()) {
                    self.echo_blob_from_guest = None;
                    return SyncAction::Nothing;
                }
                self.echo_blob_from_phone = Some(blob.id.clone());
                self.replace_phone_clipboard(PhoneClipboard::Blob(blob.clone()));
                SyncAction::PushBlobToPhone { blob }
            }
            GuestEvent::TransferBlobFailed => {
                self.awaiting_guest_transfer = None;
                self.pending_guest_mime = None;
                SyncAction::Nothing
            }
            // Always answer. wprsd has taken the pipe fd; leaving it
            // unwritten and unclosed hangs the pasting guest app forever.
            GuestEvent::PasteRequested { mime } => match &self.phone_clipboard {
                Some(PhoneClipboard::Text(text)) if is_text_mime(&mime) => {
                    SyncAction::AnswerGuest {
                        bytes: text.as_bytes().to_vec(),
                    }
                }
                Some(PhoneClipboard::Blob(blob))
                    if normalize_mime(&mime) == normalize_mime(&blob.mime) =>
                {
                    SyncAction::AnswerGuestBlob { blob: blob.clone() }
                }
                _ => SyncAction::AnswerGuest { bytes: Vec::new() },
            },
        }
    }

    /// Undoes the echo token a [`SyncAction::PushToPhone`] just installed,
    /// for when the caller learns the push reached nobody.
    ///
    /// `on_guest`'s `TransferFromGuest` arm sets `echo_from_phone`
    /// unconditionally before returning `PushToPhone`, anticipating that
    /// the phone might echo the value straight back
    /// through `on_phone_clipboard`. But the actual delivery -- fanning the
    /// message out to attached media clients -- is transport work this
    /// state machine deliberately knows nothing about (see the module
    /// comment), and that fan-out is a silent no-op when no client is
    /// attached. Left installed, a token for a push nobody received sits
    /// waiting for the *next* genuine phone copy of that same text -- which
    /// may arrive long after the phone actually attaches -- and discards it
    /// as if it were the echo of a push the guest never saw.
    ///
    /// The caller (`bridge.rs`) is the one with transport knowledge, via
    /// `MediaHub::publish_message`'s return value; this method exists so
    /// that knowledge can correct this state machine's guess without this
    /// module ever importing hub or client types itself.
    pub fn forget_phone_echo(&mut self) {
        self.echo_from_phone = None;
        self.echo_blob_from_phone = None;
    }

    pub fn on_phone_clipboard(&mut self, text: String) -> SyncAction {
        if self.echo_from_phone.as_deref() == Some(text.as_str()) {
            self.echo_from_phone = None;
            return SyncAction::Nothing;
        }

        self.echo_from_guest = Some(text.clone());
        self.replace_phone_clipboard(PhoneClipboard::Text(text));
        SyncAction::OfferToGuest {
            mime_types: OFFERED_MIME_TYPES
                .iter()
                .map(|mime| (*mime).to_string())
                .collect(),
        }
    }

    pub fn on_phone_blob(&mut self, blob: BlobDescriptor) -> SyncAction {
        if blob.validate().is_err() {
            return SyncAction::Nothing;
        }
        if self.echo_blob_from_phone.as_deref() == Some(blob.id.as_str()) {
            self.echo_blob_from_phone = None;
            return SyncAction::Nothing;
        }

        self.echo_blob_from_guest = Some(blob.id.clone());
        self.replace_phone_clipboard(PhoneClipboard::Blob(blob));
        SyncAction::OfferToGuest {
            mime_types: match &self.phone_clipboard {
                Some(PhoneClipboard::Blob(blob)) => vec![blob.mime.clone()],
                _ => unreachable!("the blob clipboard was just stored"),
            },
        }
    }

    /// The bridge asks this before turning raw WPRS transfer bytes into a
    /// stored descriptor. Exposing only MIME metadata keeps I/O outside this
    /// decision layer.
    pub fn pending_guest_blob_mime(&self) -> Option<String> {
        (self.awaiting_guest_transfer == Some(AwaitingGuestTransfer::Blob))
            .then(|| self.pending_guest_mime.clone())
            .flatten()
    }

    /// Returns blobs displaced by newer clipboard state. The bridge owns the
    /// storage lifetime and must attempt the safe descriptor-checked delete.
    pub fn take_retired_blobs(&mut self) -> Vec<BlobDescriptor> {
        std::mem::take(&mut self.retired_blobs)
    }

    fn replace_phone_clipboard(&mut self, next: PhoneClipboard) {
        let next_blob_id = match &next {
            PhoneClipboard::Blob(blob) => Some(blob.id.clone()),
            PhoneClipboard::Text(_) => None,
        };
        if let Some(PhoneClipboard::Blob(previous)) = self.phone_clipboard.replace(next)
            && Some(previous.id.as_str()) != next_blob_id.as_deref()
        {
            self.retired_blobs.push(previous);
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

fn select_image_mime(offered: &[String]) -> Option<String> {
    OFFERED_IMAGE_MIME_TYPES.iter().find_map(|preferred| {
        offered
            .iter()
            .find(|candidate| normalize_mime(candidate) == *preferred)
            .cloned()
    })
}

fn is_text_mime(mime: &str) -> bool {
    select_text_mime(&[mime.to_string()]).is_some()
}

fn is_image_mime(mime: &str) -> bool {
    OFFERED_IMAGE_MIME_TYPES
        .iter()
        .any(|candidate| normalize_mime(mime) == *candidate)
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
    fn an_offer_with_no_supported_form_is_ignored() {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_guest(offer(&["application/pdf"])),
            SyncAction::Nothing
        );
    }

    /// A second-opinion review found this: the `None` branch of
    /// `SelectionOffered` left `awaiting_guest_transfer` set, so a transfer
    /// that lands late for a selection the guest has already replaced with
    /// one carrying no text form is answered as if it belonged to a live
    /// request, overwriting the phone's clipboard from a selection that no
    /// longer has any text.
    #[test]
    fn a_non_text_offer_disarms_a_transfer_still_in_flight_for_the_selection_it_replaced() {
        let mut sync = ClipboardSync::new();
        sync.on_guest(offer(&["text/plain"]));
        // The guest replaces its selection with an unsupported offer before
        // the transfer for the old, text-bearing offer lands.
        assert_eq!(
            sync.on_guest(offer(&["application/pdf"])),
            SyncAction::Nothing
        );
        // Bytes belonging to the superseded offer arrive late. They must
        // be dropped, not answered as though a request were still live.
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"stale".to_vec()
            }),
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

    /// A controller review caught this reappearing: Task 5 deferred it as
    /// handled downstream, which was true before `phone_text` was written on
    /// this exact path. An empty transfer must not become the phone's
    /// clipboard, or every later guest paste is answered with empty bytes
    /// until the phone genuinely copies something.
    #[test]
    fn an_empty_guest_transfer_is_dropped_and_does_not_become_phone_text() {
        let mut sync = ClipboardSync::new();
        sync.on_phone_clipboard("earlier".into());
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest { bytes: Vec::new() }),
            SyncAction::Nothing
        );
        assert_eq!(
            sync.on_guest(GuestEvent::PasteRequested {
                mime: "text/plain".into()
            }),
            SyncAction::AnswerGuest {
                bytes: b"earlier".to_vec()
            },
            "an empty transfer must not overwrite phone_text with an empty value"
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
            sync.on_guest(GuestEvent::PasteRequested {
                mime: "text/plain".into()
            }),
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
            sync.on_guest(GuestEvent::PasteRequested {
                mime: "text/plain".into()
            }),
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

    /// A second-opinion review found this: `echo_from_phone` is set
    /// unconditionally before `PushToPhone` is returned, but the daemon's
    /// own fan-out is a silent no-op when no phone client is attached. A
    /// guest copy made while the phone is disconnected installs a token
    /// nobody will ever consume as an echo -- and the next genuine phone
    /// copy of that same text, however much later, is mistaken for that
    /// phantom echo and silently discarded instead of reaching the guest.
    /// `forget_phone_echo` is what `bridge.rs` calls once it learns the
    /// push reached zero clients, undoing the guess this state machine had
    /// to make before that was known.
    #[test]
    fn forgetting_an_unreceived_push_lets_the_next_genuine_phone_copy_through() {
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
        // The caller learns no client was attached to receive it.
        sync.forget_phone_echo();

        // The phone later attaches and the user genuinely copies "hello".
        // Without forgetting the token above, this would be mistaken for
        // the echo of a push the phone never actually received.
        assert_eq!(
            sync.on_phone_clipboard("hello".into()),
            SyncAction::OfferToGuest {
                mime_types: OFFERED_MIME_TYPES
                    .iter()
                    .map(|m| (*m).to_string())
                    .collect()
            },
            "a genuine phone copy must not be mistaken for the echo of a push nobody received"
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

    /// `phone_text` must track the phone's actual clipboard, including
    /// values that arrived via a guest-to-phone push, not just values set
    /// directly by `on_phone_clipboard`. A paste answered from a stale
    /// `phone_text` hands the guest data the phone no longer has.
    #[test]
    fn a_paste_after_a_guest_push_answers_with_the_pushed_text_not_a_stale_value() {
        let mut sync = ClipboardSync::new();
        sync.on_phone_clipboard("foo".into());
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"bar".to_vec()
            }),
            SyncAction::PushToPhone { text: "bar".into() }
        );
        assert_eq!(
            sync.on_guest(GuestEvent::PasteRequested {
                mime: "text/plain".into()
            }),
            SyncAction::AnswerGuest {
                bytes: b"bar".to_vec()
            },
            "phone_text must reflect the value just pushed to the phone, not the earlier one"
        );
    }

    fn blob() -> BlobDescriptor {
        BlobDescriptor {
            id: "0123456789abcdef0123456789abcdef".into(),
            mime: "image/png".into(),
            size: 42,
        }
    }

    #[test]
    fn text_is_preferred_when_a_guest_offers_text_and_an_image() {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_guest(offer(&["image/png", "text/plain"])),
            SyncAction::AskGuestFor {
                mime: "text/plain".into()
            }
        );
    }

    #[test]
    fn a_guest_image_transfer_is_pushed_as_a_descriptor_and_echoes_once() {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_guest(offer(&["image/png"])),
            SyncAction::AskGuestFor {
                mime: "image/png".into()
            }
        );
        assert_eq!(
            sync.on_guest(GuestEvent::TransferBlobFromGuest { blob: blob() }),
            SyncAction::PushBlobToPhone { blob: blob() }
        );
        assert_eq!(sync.on_phone_blob(blob()), SyncAction::Nothing);
        assert_eq!(
            sync.on_phone_blob(blob()),
            SyncAction::OfferToGuest {
                mime_types: vec!["image/png".into()]
            }
        );
    }

    #[test]
    fn superseding_a_blob_records_it_for_bridge_reclamation() {
        let mut sync = ClipboardSync::new();
        assert!(matches!(
            sync.on_phone_blob(blob()),
            SyncAction::OfferToGuest { .. }
        ));
        sync.on_phone_clipboard("new text".into());

        assert_eq!(sync.take_retired_blobs(), vec![blob()]);
        assert!(
            sync.take_retired_blobs().is_empty(),
            "retired blobs are drained once"
        );
    }

    #[test]
    fn a_blob_paste_never_claims_bytes_when_the_bridge_cannot_find_the_blob() {
        let mut sync = ClipboardSync::new();
        sync.on_phone_blob(blob());
        assert_eq!(
            sync.on_guest(GuestEvent::PasteRequested {
                mime: "image/png".into()
            }),
            SyncAction::AnswerGuestBlob { blob: blob() },
            "the bridge turns a missing descriptor into an empty pipe transfer"
        );
        assert_eq!(
            sync.on_guest(GuestEvent::PasteRequested {
                mime: "text/plain".into()
            }),
            SyncAction::AnswerGuest { bytes: Vec::new() },
        );
    }
}

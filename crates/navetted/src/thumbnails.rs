//! Session thumbnails for the drawer: what a session looked like the last
//! time anyone saw it.
//!
//! Two pieces. [`ThumbnailStore`] is the daemon-wide map the API reads from:
//! one JPEG per session, in memory only, gone on restart. [`ThumbnailCapture`]
//! is the per-session policy that decides *when* to put one there, and it
//! runs on the encode thread, where frames already are and where a few
//! milliseconds every ten seconds cost nobody anything.
//!
//! Memory per session is bounded by one JPEG in the store (~10-30 KB) plus
//! one downscaled RGB image in the capture (at most [`THUMBNAIL_MAX_WIDTH`]
//! wide, so ~170 KB for a 16:9 window; the frame it came from is never
//! kept). An idle session parks that and nothing more.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use navette_bridge::{
    Frame, RgbImage, SurfaceKey, THUMBNAIL_MAX_WIDTH, Thumbnail, ThumbnailError,
    downscale_bgra_to_rgb, thumbnail_from_frame,
};

/// How often a session's thumbnail is refreshed while frames are flowing.
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(10);
/// The retained frame is replaced at most this often. It is a full-size
/// clone, so this bounds the copy cost to about one memcpy a second.
pub const KEEP_INTERVAL: Duration = Duration::from_secs(1);
/// A retained frame older than this is replaced by whatever arrives next,
/// even a smaller window: the larger one has stopped painting, and a stale
/// picture of it is worth less than a fresh picture of what is active.
pub const KEEP_STALE: Duration = Duration::from_secs(2);
/// A detach snapshot is skipped while the last snapshot is younger than
/// this, so an attach/detach loop cannot force a JPEG encode per detach.
pub const DETACH_SNAPSHOT_FLOOR: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct StoredThumbnail {
    pub jpeg: Bytes,
    /// Strong validator for `If-None-Match`, already quoted.
    pub etag: String,
    pub taken_at: SystemTime,
    pub width: u32,
    pub height: u32,
}

/// One thumbnail per session name, so memory is bounded by the session
/// count (~30 KB each) rather than by uptime.
#[derive(Clone, Default)]
pub struct ThumbnailStore(Arc<StoreInner>);

#[derive(Default)]
struct StoreInner {
    entries: Mutex<HashMap<String, Arc<StoredThumbnail>>>,
    /// Every `put` takes the next value, so two snapshots can never share an
    /// etag however close together they land -- a millisecond clock could
    /// not promise that.
    sequence: AtomicU64,
}

impl ThumbnailStore {
    pub fn put(&self, session: &str, thumbnail: Thumbnail) {
        let sequence = self.0.sequence.fetch_add(1, Ordering::Relaxed);
        let stored = Arc::new(StoredThumbnail {
            etag: etag_for(sequence, thumbnail.jpeg.len()),
            jpeg: Bytes::from(thumbnail.jpeg),
            taken_at: SystemTime::now(),
            width: thumbnail.width,
            height: thumbnail.height,
        });
        if let Ok(mut entries) = self.0.entries.lock() {
            entries.insert(session.to_owned(), stored);
        }
    }

    pub fn get(&self, session: &str) -> Option<Arc<StoredThumbnail>> {
        self.0.entries.lock().ok()?.get(session).cloned()
    }

    pub fn remove(&self, session: &str) {
        if let Ok(mut entries) = self.0.entries.lock() {
            entries.remove(session);
        }
    }
}

/// `"<sequence hex>-<len hex>"`. The sequence alone makes it unique; the
/// length is kept so the tag says something about the body it names.
fn etag_for(sequence: u64, len: usize) -> String {
    format!("\"{sequence:x}-{len:x}\"")
}

/// The picture retained for a snapshot after frames stop arriving: the
/// downscaled image, not the frame. Downscaling costs a few milliseconds and
/// happens at most once per [`KEEP_INTERVAL`]; keeping the frame instead
/// would park 8 MB per idle 1080p session for nothing.
struct KeptPicture {
    key: SurfaceKey,
    image: RgbImage,
    /// Area of the frame `image` was made from, so "largest toplevel" is
    /// still judged on window size after both have been scaled to 320 wide.
    source_area: u64,
    kept_at: Instant,
}

fn frame_area(frame: &Frame) -> u64 {
    u64::from(frame.width) * u64::from(frame.height)
}

/// Decides when one session's thumbnail is refreshed.
///
/// Fed every frame the encode thread handles. It keeps at most one retained
/// picture -- preferring the largest toplevel, since that is the window the
/// drawer should show -- and refreshes the store every
/// [`SNAPSHOT_INTERVAL`] while frames flow. The retained picture exists so a
/// snapshot can still be taken *after* frames stop: when the last client
/// detaches, the bridge asks for one and gets the most recent picture rather
/// than nothing.
///
/// Time is passed in rather than read here, so the policy is testable
/// without waiting ten real seconds.
pub struct ThumbnailCapture {
    session: String,
    store: ThumbnailStore,
    kept: Option<KeptPicture>,
    last_snapshot: Option<Instant>,
}

impl ThumbnailCapture {
    pub fn new(session: impl Into<String>, store: ThumbnailStore) -> Self {
        Self {
            session: session.into(),
            store,
            kept: None,
            last_snapshot: None,
        }
    }

    /// Records a composited frame. Returns whether a snapshot was taken.
    ///
    /// The frame is downscaled into the retained slot at most once per
    /// [`KEEP_INTERVAL`], and only if it is at least as large as what is
    /// kept or the kept picture has gone stale. A snapshot is taken from the
    /// larger of this frame and the kept one when [`SNAPSHOT_INTERVAL`] has
    /// passed -- or immediately on the first frame, so a fresh session shows
    /// something in the drawer without a ten-second wait.
    pub fn observe(&mut self, key: SurfaceKey, frame: &Frame, now: Instant) -> bool {
        let area = frame_area(frame);
        if area == 0 {
            return false;
        }
        let retained = self.retain(key, frame, now);
        if !self.snapshot_due(now) {
            return false;
        }
        let thumbnail = match &self.kept {
            // Just downscaled from this very frame, or a fresh picture of a
            // larger window: either way the kept image is the one to encode,
            // and the frame need not be scaled a second time.
            Some(kept) if retained || kept.source_area > area => Thumbnail::from_rgb(&kept.image),
            _ => thumbnail_from_frame(frame),
        };
        self.put_snapshot(thumbnail, now)
    }

    /// Snapshots the retained picture outside the periodic schedule -- the
    /// last client just detached. A no-op that returns `false` when nothing
    /// has been retained (a session that never painted has nothing to show)
    /// or when the last snapshot is younger than [`DETACH_SNAPSHOT_FLOOR`],
    /// so a client attaching and detaching in a loop cannot make this thread
    /// encode a JPEG per detach.
    pub fn snapshot_kept(&mut self, now: Instant) -> bool {
        if self
            .last_snapshot
            .is_some_and(|last| now.saturating_duration_since(last) < DETACH_SNAPSHOT_FLOOR)
        {
            return false;
        }
        let Some(kept) = &self.kept else {
            return false;
        };
        let thumbnail = Thumbnail::from_rgb(&kept.image);
        self.put_snapshot(thumbnail, now)
    }

    /// Drops the retained picture if it belongs to `key`: its window is
    /// gone, and a detach snapshot must not resurrect it.
    pub fn forget_surface(&mut self, key: SurfaceKey) {
        if self.kept.as_ref().is_some_and(|kept| kept.key == key) {
            self.kept = None;
        }
    }

    /// Drops the retained picture if its window belonged to `client_id`.
    pub fn forget_client(&mut self, client_id: u64) {
        if self
            .kept
            .as_ref()
            .is_some_and(|kept| kept.key.client_id == client_id)
        {
            self.kept = None;
        }
    }

    /// Bytes of picture currently parked for a later snapshot.
    #[cfg(test)]
    fn retained_bytes(&self) -> usize {
        self.kept.as_ref().map_or(0, |kept| kept.image.pixels.len())
    }

    /// Replaces the retained picture with a downscale of `frame` when the
    /// policy says so, returning whether it did.
    fn retain(&mut self, key: SurfaceKey, frame: &Frame, now: Instant) -> bool {
        let area = frame_area(frame);
        let replace = match &self.kept {
            None => true,
            Some(kept) => {
                let age = now.saturating_duration_since(kept.kept_at);
                age >= KEEP_STALE || (age >= KEEP_INTERVAL && area >= kept.source_area)
            }
        };
        if !replace {
            return false;
        }
        match downscale_bgra_to_rgb(frame, THUMBNAIL_MAX_WIDTH) {
            Ok(image) => {
                self.kept = Some(KeptPicture {
                    key,
                    image,
                    source_area: area,
                    kept_at: now,
                });
                true
            }
            Err(error) => {
                tracing::debug!(session = %self.session, %error, "frame not retained for a thumbnail");
                false
            }
        }
    }

    fn snapshot_due(&self, now: Instant) -> bool {
        self.last_snapshot
            .is_none_or(|last| now.saturating_duration_since(last) >= SNAPSHOT_INTERVAL)
    }

    /// Stores an encoded thumbnail and stamps the snapshot time. Failure is
    /// reported rather than propagated: a thumbnail that could not be taken
    /// is a missing tile in a drawer, not a broken session.
    fn put_snapshot(&mut self, thumbnail: Result<Thumbnail, ThumbnailError>, now: Instant) -> bool {
        match thumbnail {
            Ok(thumbnail) => {
                self.store.put(&self.session, thumbnail);
                self.last_snapshot = Some(now);
                true
            }
            Err(error) => {
                tracing::debug!(session = %self.session, %error, "session thumbnail not taken");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thumbnail(len: usize) -> Thumbnail {
        Thumbnail {
            jpeg: vec![0xab; len],
            width: 320,
            height: 180,
        }
    }

    fn frame(width: u32, height: u32) -> Frame {
        Frame {
            width,
            height,
            pixels: vec![0x7f; width as usize * height as usize * 4],
        }
    }

    fn key(surface_id: u64) -> SurfaceKey {
        SurfaceKey {
            client_id: 1,
            surface_id,
        }
    }

    #[test]
    fn put_get_remove_round_trip() {
        let store = ThumbnailStore::default();
        assert!(store.get("work").is_none());

        store.put("work", thumbnail(16));
        let stored = store.get("work").expect("stored after put");
        assert_eq!(stored.jpeg.len(), 16);
        assert_eq!((stored.width, stored.height), (320, 180));
        assert!(stored.etag.starts_with('"') && stored.etag.ends_with('"'));
        assert!(store.get("other").is_none(), "keyed by session name");

        store.remove("work");
        assert!(store.get("work").is_none());
    }

    #[test]
    fn a_second_put_replaces_the_thumbnail_and_changes_the_etag() {
        let store = ThumbnailStore::default();
        store.put("work", thumbnail(16));
        let first = store.get("work").unwrap();

        // A different length guarantees a different etag even when both
        // puts land in the same millisecond.
        store.put("work", thumbnail(24));
        let second = store.get("work").unwrap();

        assert_eq!(second.jpeg.len(), 24);
        assert_ne!(first.etag, second.etag);
        assert_ne!(
            Arc::as_ptr(&first),
            Arc::as_ptr(&second),
            "a put replaces the entry rather than mutating it"
        );
    }

    #[test]
    fn etags_are_unique_across_puts_even_with_identical_bodies() {
        assert_eq!(etag_for(0x1234, 0xff), "\"1234-ff\"");

        let store = ThumbnailStore::default();
        store.put("a", thumbnail(16));
        store.put("b", thumbnail(16));
        let first = store.get("a").unwrap().etag.clone();
        let second = store.get("b").unwrap().etag.clone();
        assert_ne!(
            first, second,
            "same body, same instant, still distinct tags"
        );

        store.put("a", thumbnail(16));
        assert_ne!(
            store.get("a").unwrap().etag,
            first,
            "a re-put of an identical body still changes the tag"
        );
    }

    #[test]
    fn two_frames_half_a_second_apart_snapshot_once() {
        let store = ThumbnailStore::default();
        let mut capture = ThumbnailCapture::new("s1", store.clone());
        let start = Instant::now();

        assert!(
            capture.observe(key(1), &frame(64, 64), start),
            "the first frame snapshots immediately"
        );
        assert!(!capture.observe(key(1), &frame(64, 64), start + Duration::from_millis(500)));

        assert!(store.get("s1").is_some());
    }

    #[test]
    fn frames_spanning_the_interval_snapshot_twice() {
        let store = ThumbnailStore::default();
        let mut capture = ThumbnailCapture::new("s1", store.clone());
        let start = Instant::now();

        assert!(capture.observe(key(1), &frame(64, 64), start));
        assert!(!capture.observe(key(1), &frame(64, 64), start + Duration::from_secs(5)));
        assert!(!capture.observe(
            key(1),
            &frame(64, 64),
            start + SNAPSHOT_INTERVAL - Duration::from_millis(1)
        ));
        assert!(capture.observe(key(1), &frame(64, 64), start + SNAPSHOT_INTERVAL));
        assert!(!capture.observe(
            key(1),
            &frame(64, 64),
            start + SNAPSHOT_INTERVAL + Duration::from_secs(1)
        ));
    }

    #[test]
    fn snapshot_kept_uses_the_retained_frame_and_is_a_no_op_without_one() {
        let store = ThumbnailStore::default();
        let mut capture = ThumbnailCapture::new("s1", store.clone());
        let start = Instant::now();

        assert!(!capture.snapshot_kept(start), "nothing retained yet");
        assert!(store.get("s1").is_none());

        capture.observe(key(1), &frame(128, 64), start);
        store.remove("s1");
        assert!(capture.snapshot_kept(start + Duration::from_secs(1)));
        let stored = store.get("s1").expect("snapshot from the kept frame");
        assert_eq!((stored.width, stored.height), (128, 64));
    }

    #[test]
    fn the_largest_toplevel_wins_when_two_keys_alternate() {
        let store = ThumbnailStore::default();
        let mut capture = ThumbnailCapture::new("s1", store.clone());
        let start = Instant::now();
        let big = frame(256, 128);
        let small = frame(64, 32);

        // The big window arrives first; the small one keeps painting.
        capture.observe(key(1), &big, start);
        for step in 1..=5 {
            capture.observe(key(2), &small, start + Duration::from_millis(300 * step));
        }
        store.remove("s1");
        assert!(capture.snapshot_kept(start + Duration::from_secs(2)));
        let stored = store.get("s1").unwrap();
        assert_eq!(
            (stored.width, stored.height),
            (256, 128),
            "a smaller window must not displace a fresh larger one"
        );

        // Once the big window has been quiet past the stale bound, the small
        // one takes over: a fresh picture of the active window beats a
        // stale one of the idle window.
        capture.observe(key(2), &small, start + Duration::from_secs(3));
        store.remove("s1");
        assert!(capture.snapshot_kept(start + Duration::from_secs(3)));
        let stored = store.get("s1").unwrap();
        assert_eq!((stored.width, stored.height), (64, 32));

        // And the big window coming back displaces the small one as soon as
        // the keep interval allows, without waiting for staleness.
        capture.observe(key(1), &big, start + Duration::from_secs(4));
        store.remove("s1");
        assert!(capture.snapshot_kept(start + Duration::from_secs(4)));
        let stored = store.get("s1").unwrap();
        assert_eq!((stored.width, stored.height), (256, 128));
    }

    #[test]
    fn a_periodic_snapshot_prefers_the_kept_larger_frame_over_a_small_incoming_one() {
        let store = ThumbnailStore::default();
        let mut capture = ThumbnailCapture::new("s1", store.clone());
        let start = Instant::now();

        capture.observe(key(1), &frame(256, 128), start);
        // The big window repaints just before the interval elapses, so it is
        // still fresh (well inside `KEEP_STALE`) when a small window's frame
        // is the one that trips the snapshot.
        assert!(!capture.observe(
            key(1),
            &frame(256, 128),
            start + Duration::from_millis(9_500)
        ));
        assert!(capture.observe(key(2), &frame(64, 32), start + Duration::from_secs(10)));
        let stored = store.get("s1").unwrap();
        assert_eq!((stored.width, stored.height), (256, 128));
    }

    #[test]
    fn retention_is_rate_limited_to_the_keep_interval() {
        let store = ThumbnailStore::default();
        let mut capture = ThumbnailCapture::new("s1", store.clone());
        let start = Instant::now();

        capture.observe(key(1), &frame(64, 64), start);
        // Same size, half a second later: within the keep interval, so the
        // clone is skipped and the kept frame keeps its original stamp.
        capture.observe(key(1), &frame(64, 64), start + Duration::from_millis(500));
        assert_eq!(capture.kept.as_ref().unwrap().kept_at, start);

        capture.observe(key(1), &frame(64, 64), start + Duration::from_secs(1));
        assert_eq!(
            capture.kept.as_ref().unwrap().kept_at,
            start + Duration::from_secs(1)
        );
    }

    #[test]
    fn forgetting_a_surface_or_client_drops_the_kept_frame() {
        let store = ThumbnailStore::default();
        let mut capture = ThumbnailCapture::new("s1", store.clone());
        let start = Instant::now();

        capture.observe(key(1), &frame(64, 64), start);
        capture.forget_surface(key(2));
        assert!(
            capture.kept.is_some(),
            "another surface's destroy is ignored"
        );
        capture.forget_surface(key(1));
        assert!(capture.kept.is_none());
        assert!(!capture.snapshot_kept(start));

        capture.observe(key(1), &frame(64, 64), start + Duration::from_secs(20));
        capture.forget_client(2);
        assert!(capture.kept.is_some());
        capture.forget_client(1);
        assert!(capture.kept.is_none());
    }

    #[test]
    fn a_degenerate_frame_is_ignored() {
        let store = ThumbnailStore::default();
        let mut capture = ThumbnailCapture::new("s1", store.clone());

        assert!(!capture.observe(key(1), &frame(0, 0), Instant::now()));
        assert!(capture.kept.is_none());
        assert!(store.get("s1").is_none());
    }

    /// Item 2: the retained picture must be the downscaled image, never the
    /// full-size frame -- an idle 4K session would otherwise park 33 MB.
    #[test]
    fn the_retained_picture_is_bounded_by_the_thumbnail_size_not_the_frame() {
        let mut capture = ThumbnailCapture::new("s1", ThumbnailStore::default());
        capture.observe(key(1), &frame(1920, 1080), Instant::now());

        let retained = capture.retained_bytes();
        assert!(
            retained <= 320 * 180 * 3,
            "retained {retained} bytes for a 1080p frame; the kept picture must be the \
             downscaled RGB image (<= 320x180x3), not the BGRA frame"
        );
    }

    /// Item 5: a detach snapshot is refused while the last snapshot is fresh,
    /// so an attach/detach loop cannot force a JPEG encode per detach.
    #[test]
    fn a_detach_snapshot_is_skipped_within_a_second_of_the_last_one() {
        let store = ThumbnailStore::default();
        let mut capture = ThumbnailCapture::new("s1", store.clone());
        let start = Instant::now();

        assert!(capture.observe(key(1), &frame(64, 64), start));
        store.remove("s1");
        assert!(
            !capture.snapshot_kept(start + Duration::from_millis(500)),
            "half a second after a snapshot, a detach must not take another"
        );
        assert!(store.get("s1").is_none());
        assert!(capture.snapshot_kept(start + DETACH_SNAPSHOT_FLOOR));
        assert!(store.get("s1").is_some());
        store.remove("s1");
        assert!(
            !capture.snapshot_kept(start + DETACH_SNAPSHOT_FLOOR + Duration::from_millis(1)),
            "the floor applies to a detach snapshot too, not only to periodic ones"
        );
    }
}

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use wprs::filtering;
use wprs::serialization::geometry::Rectangle;
use wprs::serialization::wayland::{
    BufferAssignment, BufferData, BufferFormat, CursorImage, Role, SurfaceRequest,
    SurfaceRequestPayload, SurfaceState,
};
use wprs::serialization::xdg_shell::{
    PopupRequest, PopupRequestPayload, ToplevelRequest, ToplevelRequestPayload,
};
use wprs::serialization::{Capabilities, ClientId, RecvType, Request};

const MAX_DIMENSION: u32 = 8192;
const MAX_BUFFER_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SurfaceKey {
    pub client_id: u64,
    pub surface_id: u64,
}

impl SurfaceKey {
    fn new(client: ClientId, surface: wprs::serialization::wayland::WlSurfaceId) -> Self {
        Self {
            client_id: client.0,
            surface_id: surface.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelFormat {
    Argb8888,
    Xrgb8888,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed BGRA pixels.
    pub pixels: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SceneEvent {
    SurfaceCommitted(SurfaceKey),
    SurfaceDestroyed(SurfaceKey),
    ClientDisconnected(u64),
    CursorChanged,
    CapabilitiesChanged,
}

#[derive(Debug, Error, PartialEq)]
pub enum SceneError {
    #[error("received a second raw buffer before the first was consumed")]
    UnpairedRawBuffer,
    #[error("surface envelope and state identities differ")]
    IdentityMismatch,
    #[error("external surface buffer has no preceding raw-buffer message")]
    MissingRawBuffer,
    #[error("inline compressed buffers are unsupported")]
    InlineCompressedBuffer,
    #[error("invalid buffer metadata: {0}")]
    InvalidBuffer(String),
    #[error("surface is unknown: {0:?}")]
    UnknownSurface(SurfaceKey),
    #[error("surface is not a toplevel: {0:?}")]
    NotToplevel(SurfaceKey),
    #[error("surface hierarchy contains a cycle")]
    SurfaceCycle,
}

#[derive(Clone, Debug)]
struct Image {
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    pixels: Vec<u8>,
}

#[derive(Clone, Debug)]
enum SurfaceRole {
    None,
    Cursor,
    Subsurface,
    Toplevel {
        title: Option<String>,
        app_id: Option<String>,
    },
    Popup {
        parent: SurfaceKey,
        x: i32,
        y: i32,
    },
}

#[derive(Clone, Debug)]
struct Child {
    key: SurfaceKey,
    x: i32,
    y: i32,
}

#[derive(Clone, Debug)]
struct SurfaceNode {
    role: SurfaceRole,
    children: Vec<Child>,
    /// The surface this one currently composites into, denormalized from
    /// whichever surface's `children` last listed this key -- see
    /// `Scene::parent_of` for why a subsurface can't just carry this in its
    /// own committed state and why this needs active maintenance instead of
    /// being read once.
    parent: Option<SurfaceKey>,
    image: Option<Image>,
    damage: Vec<Rectangle<i32>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToplevelInfo {
    pub key: SurfaceKey,
    pub title: Option<String>,
    pub app_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct Scene {
    surfaces: BTreeMap<SurfaceKey, SurfaceNode>,
    /// Parent assignments a commit named for a child that hadn't committed
    /// yet -- a parent can legitimately list a child surface before that
    /// surface's own first commit arrives. Consumed the moment that child
    /// does commit; see `sync_child_back_pointers` and the `Commit` arm of
    /// `apply_surface`.
    pending_parents: BTreeMap<SurfaceKey, SurfaceKey>,
    pending_raw_buffer: Option<Vec<u8>>,
    cursor: Option<CursorImage>,
    capabilities: Option<Capabilities>,
}

impl Scene {
    pub fn apply(&mut self, message: RecvType<Request>) -> Result<Vec<SceneEvent>, SceneError> {
        match message {
            RecvType::RawBuffer(buffer) => {
                if self.pending_raw_buffer.is_some() {
                    self.pending_raw_buffer = None;
                    return Err(SceneError::UnpairedRawBuffer);
                }
                self.pending_raw_buffer = Some(buffer);
                Ok(Vec::new())
            }
            RecvType::Object(request) => {
                let had_pending_raw_buffer = self.pending_raw_buffer.is_some();
                if had_pending_raw_buffer && !consumes_raw_buffer(&request) {
                    self.pending_raw_buffer = None;
                    return Err(SceneError::UnpairedRawBuffer);
                }
                let result = self.apply_request(request);
                if had_pending_raw_buffer && result.is_err() {
                    self.pending_raw_buffer = None;
                }
                result
            }
        }
    }

    pub fn surface_count(&self) -> usize {
        self.surfaces.len()
    }

    pub fn has_pending_raw_buffer(&self) -> bool {
        self.pending_raw_buffer.is_some()
    }

    pub fn toplevels(&self) -> Vec<SurfaceKey> {
        self.surfaces
            .iter()
            .filter_map(|(key, node)| {
                matches!(node.role, SurfaceRole::Toplevel { .. }).then_some(*key)
            })
            .collect()
    }

    pub fn toplevel_info(&self) -> Vec<ToplevelInfo> {
        self.surfaces
            .iter()
            .filter_map(|(key, node)| match &node.role {
                SurfaceRole::Toplevel { title, app_id } => Some(ToplevelInfo {
                    key: *key,
                    title: title.clone(),
                    app_id: app_id.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// Resolves `key` to the toplevel whose composited frame it contributes
    /// to: `key` itself when it is already a toplevel, otherwise its nearest
    /// toplevel ancestor, since [`Scene::compose_toplevel`] draws subsurface
    /// and popup children into their toplevel's frame.
    ///
    /// Returns `None` when no toplevel reaches the surface -- a cursor
    /// surface, a surface belonging to no window, or a child that has
    /// committed before its parent listed it -- and when the hierarchy
    /// contains a cycle.
    pub fn toplevel_ancestor(&self, key: SurfaceKey) -> Option<SurfaceKey> {
        let mut current = key;
        let mut visited = BTreeSet::new();
        while visited.insert(current) {
            let node = self.surfaces.get(&current)?;
            if matches!(node.role, SurfaceRole::Toplevel { .. }) {
                return Some(current);
            }
            current = self.parent_of(node)?;
        }
        None
    }

    /// The surface `key` composites into. A popup names its parent in its own
    /// role; every other child relies on `SurfaceNode::parent`, kept current
    /// by `sync_child_back_pointers` on every commit of a `children` list
    /// (a subsurface's own committed state does not carry this).
    fn parent_of(&self, node: &SurfaceNode) -> Option<SurfaceKey> {
        if let SurfaceRole::Popup { parent, .. } = node.role
            && self.surfaces.contains_key(&parent)
        {
            return Some(parent);
        }
        // Guards against a dangling pointer into a since-destroyed parent --
        // `remove_surface` never has to reach into a destroyed node's former
        // children to clear this, because a stale value simply stops
        // resolving here.
        node.parent
            .filter(|parent| self.surfaces.contains_key(parent))
    }

    pub fn damage(&self, key: SurfaceKey) -> Option<&[Rectangle<i32>]> {
        self.surfaces.get(&key).map(|node| node.damage.as_slice())
    }

    pub fn surface_dimensions(&self, key: SurfaceKey) -> Option<(u32, u32)> {
        self.surfaces
            .get(&key)
            .and_then(|node| node.image.as_ref())
            .map(|image| (image.width, image.height))
    }

    pub fn compose_toplevel(&self, root: SurfaceKey) -> Result<Frame, SceneError> {
        let node = self
            .surfaces
            .get(&root)
            .ok_or(SceneError::UnknownSurface(root))?;
        if !matches!(node.role, SurfaceRole::Toplevel { .. }) {
            return Err(SceneError::NotToplevel(root));
        }
        let image = node
            .image
            .as_ref()
            .ok_or(SceneError::UnknownSurface(root))?;
        let mut frame = Frame {
            width: image.width,
            height: image.height,
            pixels: vec![0; image.width as usize * image.height as usize * 4],
        };
        blend_image(&mut frame, image, 0, 0);
        let mut visiting = BTreeSet::from([root]);
        self.composite_children(root, 0, 0, &mut frame, &mut visiting)?;
        Ok(frame)
    }

    fn apply_request(&mut self, request: Request) -> Result<Vec<SceneEvent>, SceneError> {
        match request {
            Request::Surface(request) => self.apply_surface(request),
            Request::Toplevel(request) => self.apply_toplevel(request),
            Request::Popup(request) => self.apply_popup(request),
            Request::ClientDisconnected(client) => {
                self.surfaces.retain(|key, _| key.client_id != client.0);
                self.pending_parents
                    .retain(|key, _| key.client_id != client.0);
                Ok(vec![SceneEvent::ClientDisconnected(client.0)])
            }
            Request::CursorImage(cursor) => {
                self.cursor = Some(cursor);
                Ok(vec![SceneEvent::CursorChanged])
            }
            Request::Capabilities(capabilities) => {
                self.capabilities = Some(capabilities);
                Ok(vec![SceneEvent::CapabilitiesChanged])
            }
            Request::Data(_) => Ok(Vec::new()),
        }
    }

    fn apply_surface(&mut self, request: SurfaceRequest) -> Result<Vec<SceneEvent>, SceneError> {
        let key = SurfaceKey::new(request.client, request.surface);
        match request.payload {
            SurfaceRequestPayload::Destroyed => Ok(self.remove_surface(key)),
            SurfaceRequestPayload::Commit(state) => {
                if key != SurfaceKey::new(state.client, state.id) {
                    return Err(SceneError::IdentityMismatch);
                }
                let previous = self.surfaces.get(&key);
                let previous_image = previous.and_then(|node| node.image.clone());
                let previous_parent = previous.and_then(|node| node.parent);
                let previous_children: Vec<SurfaceKey> = previous
                    .map(|node| node.children.iter().map(|child| child.key).collect())
                    .unwrap_or_default();
                let image = self.decode_assignment(state.buffer.as_ref(), previous_image)?;
                // wprsd always appends the surface's own id to
                // `z_ordered_children`, marking where its own buffer sits in
                // the subsurface stacking order (see upstream
                // `commit_impl` in `server/smithay_handlers.rs`). That
                // buffer is already drawn by `compose_toplevel`/the parent's
                // own blend, so treating it as a literal child here would
                // make every surface its own child and trip
                // `composite_children`'s cycle guard on every commit.
                let children: Vec<Child> = state
                    .z_ordered_children
                    .iter()
                    .filter(|child| child.id != state.id)
                    .map(|child| Child {
                        key: SurfaceKey::new(state.client, child.id),
                        x: child.position.x,
                        y: child.position.y,
                    })
                    .collect();
                let node = SurfaceNode {
                    role: role_from_state(&state),
                    // A pending assignment means some parent already listed
                    // this surface as a child before this, its first commit
                    // -- that takes priority since `previous_parent` can
                    // only be `None` in that case. Otherwise this carries
                    // the parent this surface was last told it has, across
                    // its own repaints -- nothing here re-derives it, only
                    // `sync_child_back_pointers` (from the parent's own
                    // commit) or this pending lookup ever sets it.
                    parent: self.pending_parents.remove(&key).or(previous_parent),
                    children: children.clone(),
                    image,
                    damage: state.damage.unwrap_or_default(),
                };
                self.surfaces.insert(key, node);
                self.sync_child_back_pointers(key, &previous_children, &children);
                Ok(vec![SceneEvent::SurfaceCommitted(key)])
            }
        }
    }

    fn apply_toplevel(&mut self, request: ToplevelRequest) -> Result<Vec<SceneEvent>, SceneError> {
        let key = SurfaceKey::new(request.client, request.surface);
        if matches!(request.payload, ToplevelRequestPayload::Destroyed) {
            return Ok(self.remove_surface(key));
        }
        Ok(Vec::new())
    }

    fn apply_popup(&mut self, request: PopupRequest) -> Result<Vec<SceneEvent>, SceneError> {
        let key = SurfaceKey::new(request.client, request.surface);
        let PopupRequestPayload::Destroyed = request.payload;
        Ok(self.remove_surface(key))
    }

    /// Removes `key` from the scene and, if it had a toplevel ancestor other
    /// than itself, asks the caller to recomposite that ancestor. Destroying
    /// a popup or subsurface must not leave its blended pixels ghosted into
    /// the toplevel's last-published frame forever -- before this, only an
    /// unrelated commit from some other window happened to clear it, because
    /// every commit used to re-encode every toplevel. The ancestor is
    /// resolved *before* the node and its child references are removed,
    /// since resolution needs the still-intact hierarchy.
    fn remove_surface(&mut self, key: SurfaceKey) -> Vec<SceneEvent> {
        let ancestor = self
            .toplevel_ancestor(key)
            .filter(|ancestor| *ancestor != key);
        self.surfaces.remove(&key);
        self.remove_child_references(key);
        // A surface can be destroyed before it ever commits, while some
        // parent's earlier commit still has a pending assignment waiting
        // for it -- that assignment is now for a key that will never exist.
        self.pending_parents.remove(&key);
        let mut events = vec![SceneEvent::SurfaceDestroyed(key)];
        if let Some(ancestor) = ancestor {
            events.push(SceneEvent::SurfaceCommitted(ancestor));
        }
        events
    }

    fn decode_assignment(
        &mut self,
        assignment: Option<&BufferAssignment>,
        previous: Option<Image>,
    ) -> Result<Option<Image>, SceneError> {
        match assignment {
            None => Ok(previous),
            Some(BufferAssignment::Removed) => Ok(None),
            Some(BufferAssignment::New(buffer)) => {
                let filtered = match &buffer.data {
                    BufferData::External => self
                        .pending_raw_buffer
                        .take()
                        .ok_or(SceneError::MissingRawBuffer)?,
                    BufferData::Uncompressed(data) => data.0.as_ref().to_vec(),
                    BufferData::Compressed(_) => return Err(SceneError::InlineCompressedBuffer),
                };
                decode_image(buffer.metadata, filtered).map(Some)
            }
        }
    }

    fn remove_child_references(&mut self, removed: SurfaceKey) {
        for node in self.surfaces.values_mut() {
            node.children.retain(|child| child.key != removed);
        }
    }

    /// Keeps `SurfaceNode::parent` current for the children of `parent`
    /// after its `children` list changes on commit. `children` is rebuilt
    /// wholesale on every commit rather than mutated incrementally, so this
    /// runs on every commit rather than only where a diff is detected --
    /// cheap, since it's bounded by `parent`'s own child count, not the
    /// scene's size (the reason this back-pointer exists at all: resolving
    /// an ancestor no longer means scanning every surface in the scene).
    fn sync_child_back_pointers(
        &mut self,
        parent: SurfaceKey,
        previous_children: &[SurfaceKey],
        children: &[Child],
    ) {
        let current: BTreeSet<SurfaceKey> = children.iter().map(|child| child.key).collect();
        for removed in previous_children {
            if current.contains(removed) {
                continue;
            }
            if let Some(node) = self.surfaces.get_mut(removed)
                && node.parent == Some(parent)
            {
                node.parent = None;
            }
            if self.pending_parents.get(removed) == Some(&parent) {
                self.pending_parents.remove(removed);
            }
        }
        for child in children {
            if let Some(node) = self.surfaces.get_mut(&child.key) {
                node.parent = Some(parent);
            } else {
                // Not committed yet -- its own first commit will consume
                // this and stamp `parent` onto the new node then.
                self.pending_parents.insert(child.key, parent);
            }
        }
    }

    fn composite_children(
        &self,
        parent: SurfaceKey,
        parent_x: i32,
        parent_y: i32,
        frame: &mut Frame,
        visiting: &mut BTreeSet<SurfaceKey>,
    ) -> Result<(), SceneError> {
        let node = self
            .surfaces
            .get(&parent)
            .ok_or(SceneError::UnknownSurface(parent))?;
        let mut children = node.children.clone();
        let existing = children
            .iter()
            .map(|child| child.key)
            .collect::<BTreeSet<_>>();
        children.extend(self.surfaces.iter().filter_map(|(key, node)| {
            if let SurfaceRole::Popup {
                parent: popup_parent,
                x,
                y,
            } = node.role
                && popup_parent == parent
                && !existing.contains(key)
            {
                return Some(Child { key: *key, x, y });
            }
            None
        }));

        for child in children {
            if !visiting.insert(child.key) {
                return Err(SceneError::SurfaceCycle);
            }
            let Some(child_node) = self.surfaces.get(&child.key) else {
                visiting.remove(&child.key);
                continue;
            };
            let x = parent_x.saturating_add(child.x);
            let y = parent_y.saturating_add(child.y);
            if let Some(image) = &child_node.image {
                blend_image(frame, image, x, y);
            }
            self.composite_children(child.key, x, y, frame, visiting)?;
            visiting.remove(&child.key);
        }
        Ok(())
    }
}

fn role_from_state(state: &SurfaceState) -> SurfaceRole {
    match &state.role {
        None => SurfaceRole::None,
        Some(Role::Cursor(_)) => SurfaceRole::Cursor,
        Some(Role::SubSurface(_)) => SurfaceRole::Subsurface,
        Some(Role::XdgToplevel(role)) => SurfaceRole::Toplevel {
            title: role.title.clone(),
            app_id: role.app_id.clone(),
        },
        Some(Role::XdgPopup(role)) => SurfaceRole::Popup {
            parent: SurfaceKey::new(state.client, role.parent_surface_id),
            x: role
                .positioner
                .anchor_rect
                .loc
                .x
                .saturating_add(role.positioner.offset.x),
            y: role
                .positioner
                .anchor_rect
                .loc
                .y
                .saturating_add(role.positioner.offset.y),
        },
    }
}

fn consumes_raw_buffer(request: &Request) -> bool {
    matches!(
        request,
        Request::Surface(SurfaceRequest {
            payload: SurfaceRequestPayload::Commit(SurfaceState {
                buffer: Some(BufferAssignment::New(buffer)),
                ..
            }),
            ..
        }) if matches!(buffer.data, BufferData::External)
    )
}

fn decode_image(
    metadata: wprs::serialization::wayland::BufferMetadata,
    filtered: Vec<u8>,
) -> Result<Image, SceneError> {
    let width = u32::try_from(metadata.width)
        .map_err(|_| SceneError::InvalidBuffer("width must be positive".into()))?;
    let height = u32::try_from(metadata.height)
        .map_err(|_| SceneError::InvalidBuffer("height must be positive".into()))?;
    let stride = usize::try_from(metadata.stride)
        .map_err(|_| SceneError::InvalidBuffer("stride must be positive".into()))?;
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(SceneError::InvalidBuffer(
            "dimensions are out of range".into(),
        ));
    }
    let expected = stride
        .checked_mul(height as usize)
        .ok_or_else(|| SceneError::InvalidBuffer("buffer length overflow".into()))?;
    if stride < width as usize * 4 || expected > MAX_BUFFER_BYTES || filtered.len() != expected {
        return Err(SceneError::InvalidBuffer(
            "stride or buffer length is inconsistent".into(),
        ));
    }
    let mut pixels = vec![0; expected];
    filtering::unfilter(&filtered.into(), &mut pixels);
    Ok(Image {
        width,
        height,
        stride,
        format: match metadata.format {
            BufferFormat::Argb8888 => PixelFormat::Argb8888,
            BufferFormat::Xrgb8888 => PixelFormat::Xrgb8888,
        },
        pixels,
    })
}

fn blend_image(frame: &mut Frame, image: &Image, x: i32, y: i32) {
    for source_y in 0..image.height as i32 {
        let destination_y = y.saturating_add(source_y);
        if !(0..frame.height as i32).contains(&destination_y) {
            continue;
        }
        for source_x in 0..image.width as i32 {
            let destination_x = x.saturating_add(source_x);
            if !(0..frame.width as i32).contains(&destination_x) {
                continue;
            }
            let source = source_y as usize * image.stride + source_x as usize * 4;
            let destination =
                (destination_y as usize * frame.width as usize + destination_x as usize) * 4;
            let alpha = match image.format {
                PixelFormat::Argb8888 => image.pixels[source + 3],
                PixelFormat::Xrgb8888 => 255,
            };
            for channel in 0..3 {
                let foreground = u16::from(image.pixels[source + channel]);
                let background = u16::from(frame.pixels[destination + channel]);
                frame.pixels[destination + channel] =
                    ((foreground * u16::from(alpha) + background * u16::from(255 - alpha) + 127)
                        / 255) as u8;
            }
            frame.pixels[destination + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use wprs::serialization::geometry::Point;
    use wprs::serialization::wayland::{
        Buffer, BufferMetadata, SubSurfaceState, SubsurfacePosition, WlSurfaceId,
    };
    use wprs::serialization::xdg_shell::{
        XdgPopupId, XdgPopupState, XdgPositioner, XdgToplevelState,
    };

    use super::*;

    fn state(client: u64, surface: u64, role: Option<Role>) -> SurfaceState {
        SurfaceState {
            client: ClientId(client),
            id: WlSurfaceId(surface),
            buffer: None,
            role,
            buffer_scale: 1,
            buffer_transform: None,
            opaque_region: None,
            input_region: None,
            z_ordered_children: Vec::new(),
            damage: None,
            output_ids: Vec::new(),
            viewport_state: None,
            xdg_surface_state: None,
        }
    }

    fn toplevel() -> Role {
        Role::XdgToplevel(XdgToplevelState {
            id: wprs::serialization::xdg_shell::XdgToplevelId(10),
            parent: None,
            title: Some("Test".into()),
            app_id: Some("test".into()),
            decoration_mode: None,
            maximized: None,
            fullscreen: None,
        })
    }

    fn external_buffer(width: i32, height: i32, format: BufferFormat) -> BufferAssignment {
        BufferAssignment::New(Buffer {
            metadata: BufferMetadata {
                width,
                height,
                stride: width * 4,
                format,
            },
            data: BufferData::External,
        })
    }

    fn commit(state: SurfaceState) -> RecvType<Request> {
        RecvType::Object(Request::Surface(SurfaceRequest {
            client: state.client,
            surface: state.id,
            payload: SurfaceRequestPayload::Commit(state),
        }))
    }

    #[test]
    fn pairs_raw_buffer_and_preserves_image_on_damage_only_commit() {
        let mut scene = Scene::default();
        scene
            .apply(RecvType::RawBuffer(vec![1, 2, 3, 255]))
            .unwrap();
        let mut initial = state(1, 2, Some(toplevel()));
        initial.buffer = Some(external_buffer(1, 1, BufferFormat::Xrgb8888));
        scene.apply(commit(initial)).unwrap();

        let mut damage_only = state(1, 2, Some(toplevel()));
        damage_only.damage = Some(vec![Rectangle::new(0, 0, 1, 1)]);
        scene.apply(commit(damage_only)).unwrap();

        let frame = scene
            .compose_toplevel(SurfaceKey {
                client_id: 1,
                surface_id: 2,
            })
            .unwrap();
        assert_eq!(frame.pixels, [1, 2, 3, 255]);
        assert_eq!(
            scene.damage(SurfaceKey {
                client_id: 1,
                surface_id: 2,
            }),
            Some([Rectangle::new(0, 0, 1, 1)].as_slice())
        );
        assert_eq!(scene.toplevel_info()[0].title.as_deref(), Some("Test"));
        assert_eq!(scene.toplevel_info()[0].app_id.as_deref(), Some("test"));
        assert!(!scene.has_pending_raw_buffer());
    }

    #[test]
    fn rejects_invalid_or_unpaired_buffers_without_allocating_them() {
        let mut scene = Scene::default();
        let mut missing = state(1, 2, Some(toplevel()));
        missing.buffer = Some(external_buffer(1, 1, BufferFormat::Argb8888));
        assert_eq!(
            scene.apply(commit(missing)).unwrap_err(),
            SceneError::MissingRawBuffer
        );

        scene.apply(RecvType::RawBuffer(vec![0; 4])).unwrap();
        let mut oversized = state(1, 2, Some(toplevel()));
        oversized.buffer = Some(external_buffer(9000, 1, BufferFormat::Argb8888));
        assert!(matches!(
            scene.apply(commit(oversized)),
            Err(SceneError::InvalidBuffer(_))
        ));
        assert_eq!(scene.surface_count(), 0);

        scene.apply(RecvType::RawBuffer(vec![0; 4])).unwrap();
        assert_eq!(
            scene.apply(RecvType::RawBuffer(vec![0; 4])).unwrap_err(),
            SceneError::UnpairedRawBuffer
        );
        assert!(!scene.has_pending_raw_buffer());

        scene.apply(RecvType::RawBuffer(vec![0; 4])).unwrap();
        assert_eq!(
            scene
                .apply(RecvType::Object(Request::Capabilities(Capabilities {
                    xwayland: false
                },)))
                .unwrap_err(),
            SceneError::UnpairedRawBuffer
        );
        assert!(!scene.has_pending_raw_buffer());
    }

    #[test]
    fn composites_subsurface_with_alpha_and_clipping() {
        let mut scene = Scene::default();
        scene
            .apply(RecvType::RawBuffer(vec![10, 20, 30, 40, 50, 60, 0, 0]))
            .unwrap();
        let mut root = state(1, 1, Some(toplevel()));
        root.buffer = Some(external_buffer(2, 1, BufferFormat::Xrgb8888));
        root.z_ordered_children.push(SubsurfacePosition {
            id: WlSurfaceId(2),
            position: Point { x: 1, y: 0 },
        });
        scene.apply(commit(root)).unwrap();

        scene
            .apply(RecvType::RawBuffer(vec![110, 120, 130, 128]))
            .unwrap();
        let mut child = state(
            1,
            2,
            Some(Role::SubSurface(SubSurfaceState {
                parent: WlSurfaceId(1),
                location: Point { x: 1, y: 0 },
                sync: true,
            })),
        );
        child.buffer = Some(external_buffer(1, 1, BufferFormat::Argb8888));
        scene.apply(commit(child)).unwrap();

        let frame = scene
            .compose_toplevel(SurfaceKey {
                client_id: 1,
                surface_id: 1,
            })
            .unwrap();
        assert_eq!(&frame.pixels[..4], &[10, 30, 50, 255]);
        assert_eq!(&frame.pixels[4..], &[65, 80, 95, 255]);
    }

    #[test]
    fn a_surfaces_self_entry_in_z_ordered_children_does_not_trip_the_cycle_guard() {
        // wprsd's own `commit_impl` always appends a surface's own id to its
        // `z_ordered_children`, marking where its own buffer sits in the
        // subsurface stacking order (see upstream `server/smithay_handlers.rs`).
        // Every real commit -- root or child -- carries this self-entry; a
        // synthetic fixture that omits it (as every other test here does,
        // including the otherwise-identical
        // `composites_subsurface_with_alpha_and_clipping` this mirrors)
        // can't catch a composite_children that treats it as a literal,
        // cycle-triggering child.
        let mut scene = Scene::default();
        scene
            .apply(RecvType::RawBuffer(vec![10, 20, 30, 40, 50, 60, 0, 0]))
            .unwrap();
        let mut root = state(1, 1, Some(toplevel()));
        root.buffer = Some(external_buffer(2, 1, BufferFormat::Xrgb8888));
        root.z_ordered_children.push(SubsurfacePosition {
            id: WlSurfaceId(2),
            position: Point { x: 1, y: 0 },
        });
        // The self-entry wprsd always appends, in addition to the real child.
        root.z_ordered_children.push(SubsurfacePosition {
            id: WlSurfaceId(1),
            position: Point { x: 0, y: 0 },
        });
        scene.apply(commit(root)).unwrap();

        scene
            .apply(RecvType::RawBuffer(vec![110, 120, 130, 128]))
            .unwrap();
        let mut child = state(
            1,
            2,
            Some(Role::SubSurface(SubSurfaceState {
                parent: WlSurfaceId(1),
                location: Point { x: 1, y: 0 },
                sync: true,
            })),
        );
        child.buffer = Some(external_buffer(1, 1, BufferFormat::Argb8888));
        // The child's own commit also carries wprsd's self-entry.
        child.z_ordered_children.push(SubsurfacePosition {
            id: WlSurfaceId(2),
            position: Point { x: 0, y: 0 },
        });
        scene.apply(commit(child)).unwrap();

        let frame = scene
            .compose_toplevel(SurfaceKey {
                client_id: 1,
                surface_id: 1,
            })
            .unwrap();
        assert_eq!(&frame.pixels[..4], &[10, 30, 50, 255]);
        assert_eq!(&frame.pixels[4..], &[65, 80, 95, 255]);
    }

    #[test]
    fn composites_popup_relative_to_its_parent() {
        let mut scene = Scene::default();
        scene
            .apply(RecvType::RawBuffer(vec![10, 20, 10, 20, 10, 20, 255, 255]))
            .unwrap();
        let mut root = state(1, 1, Some(toplevel()));
        root.buffer = Some(external_buffer(2, 1, BufferFormat::Xrgb8888));
        scene.apply(commit(root)).unwrap();

        scene
            .apply(RecvType::RawBuffer(vec![90, 80, 70, 255]))
            .unwrap();
        let mut popup = state(
            1,
            2,
            Some(Role::XdgPopup(XdgPopupState {
                id: XdgPopupId(11),
                parent_surface_id: WlSurfaceId(1),
                positioner: XdgPositioner {
                    width: 1,
                    height: 1,
                    anchor_rect: Rectangle::new(1, 0, 1, 1),
                    anchor_edges: 0,
                    gravity: 0,
                    constraint_adjustment: 0,
                    offset: Point { x: 0, y: 0 },
                    reactive: false,
                    parent_size: None,
                    parent_configure: None,
                },
                grab_requested: false,
            })),
        );
        popup.buffer = Some(external_buffer(1, 1, BufferFormat::Xrgb8888));
        scene.apply(commit(popup)).unwrap();

        let frame = scene
            .compose_toplevel(SurfaceKey {
                client_id: 1,
                surface_id: 1,
            })
            .unwrap();
        assert_eq!(&frame.pixels[..4], &[10, 10, 10, 255]);
        assert_eq!(&frame.pixels[4..], &[90, 80, 70, 255]);
    }

    #[test]
    fn destroying_a_popup_asks_its_toplevel_ancestor_to_recomposite() {
        let mut scene = Scene::default();
        scene
            .apply(RecvType::RawBuffer(vec![10, 20, 10, 20, 10, 20, 255, 255]))
            .unwrap();
        let mut root = state(1, 1, Some(toplevel()));
        root.buffer = Some(external_buffer(2, 1, BufferFormat::Xrgb8888));
        scene.apply(commit(root)).unwrap();

        scene
            .apply(RecvType::RawBuffer(vec![90, 80, 70, 255]))
            .unwrap();
        let mut popup = state(
            1,
            2,
            Some(Role::XdgPopup(XdgPopupState {
                id: XdgPopupId(11),
                parent_surface_id: WlSurfaceId(1),
                positioner: XdgPositioner {
                    width: 1,
                    height: 1,
                    anchor_rect: Rectangle::new(1, 0, 1, 1),
                    anchor_edges: 0,
                    gravity: 0,
                    constraint_adjustment: 0,
                    offset: Point { x: 0, y: 0 },
                    reactive: false,
                    parent_size: None,
                    parent_configure: None,
                },
                grab_requested: false,
            })),
        );
        popup.buffer = Some(external_buffer(1, 1, BufferFormat::Xrgb8888));
        scene.apply(commit(popup)).unwrap();

        let root_key = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };
        let popup_key = SurfaceKey {
            client_id: 1,
            surface_id: 2,
        };

        // The point of this fix: without it, only SurfaceDestroyed(popup) is
        // emitted, and nothing tells the bridge to re-encode the toplevel
        // the popup was blended into -- so its pixels linger in the last
        // published frame until some unrelated commit happens to clear them.
        let events = scene
            .apply(RecvType::Object(Request::Popup(PopupRequest {
                client: ClientId(1),
                surface: WlSurfaceId(2),
                payload: PopupRequestPayload::Destroyed,
            })))
            .unwrap();
        assert_eq!(
            events,
            vec![
                SceneEvent::SurfaceDestroyed(popup_key),
                SceneEvent::SurfaceCommitted(root_key),
            ]
        );

        // And the ancestor really does recomposite clean: the popup's pixels
        // are gone from the toplevel's frame, not just the event emitted.
        // (The exact background value depends on raw-buffer unfiltering, not
        // relevant here -- what matters is that it's no longer the popup's.)
        let frame = scene.compose_toplevel(root_key).unwrap();
        assert_eq!(&frame.pixels[..4], &[10, 10, 10, 255]);
        assert_ne!(&frame.pixels[4..], &[90, 80, 70, 255]);
    }

    #[test]
    fn destroying_a_toplevel_does_not_ask_it_to_recomposite_itself() {
        let mut scene = Scene::default();
        scene
            .apply(RecvType::RawBuffer(vec![10, 20, 30, 40]))
            .unwrap();
        let mut root = state(1, 1, Some(toplevel()));
        root.buffer = Some(external_buffer(1, 1, BufferFormat::Xrgb8888));
        scene.apply(commit(root)).unwrap();

        let events = scene
            .apply(RecvType::Object(Request::Toplevel(ToplevelRequest {
                client: ClientId(1),
                surface: WlSurfaceId(1),
                payload: ToplevelRequestPayload::Destroyed,
            })))
            .unwrap();
        assert_eq!(
            events,
            vec![SceneEvent::SurfaceDestroyed(SurfaceKey {
                client_id: 1,
                surface_id: 1,
            })]
        );
    }

    #[test]
    fn resolves_children_to_their_toplevel_ancestor() {
        let mut scene = Scene::default();
        scene
            .apply(RecvType::RawBuffer(vec![10, 20, 30, 40, 50, 60, 0, 0]))
            .unwrap();
        let mut root = state(1, 1, Some(toplevel()));
        root.buffer = Some(external_buffer(2, 1, BufferFormat::Xrgb8888));
        root.z_ordered_children.push(SubsurfacePosition {
            id: WlSurfaceId(2),
            position: Point { x: 1, y: 0 },
        });
        scene.apply(commit(root)).unwrap();

        scene
            .apply(RecvType::RawBuffer(vec![110, 120, 130, 128]))
            .unwrap();
        let mut child = state(
            1,
            2,
            Some(Role::SubSurface(SubSurfaceState {
                parent: WlSurfaceId(1),
                location: Point { x: 1, y: 0 },
                sync: true,
            })),
        );
        child.buffer = Some(external_buffer(1, 1, BufferFormat::Argb8888));
        scene.apply(commit(child)).unwrap();

        // A nested subsurface: the walk up is transitive, not one hop.
        let mut nested_parent = state(
            1,
            2,
            Some(Role::SubSurface(SubSurfaceState {
                parent: WlSurfaceId(1),
                location: Point { x: 1, y: 0 },
                sync: true,
            })),
        );
        nested_parent.z_ordered_children.push(SubsurfacePosition {
            id: WlSurfaceId(3),
            position: Point { x: 0, y: 0 },
        });
        scene.apply(commit(nested_parent)).unwrap();
        scene
            .apply(commit(state(
                1,
                3,
                Some(Role::SubSurface(SubSurfaceState {
                    parent: WlSurfaceId(2),
                    location: Point { x: 0, y: 0 },
                    sync: true,
                })),
            )))
            .unwrap();

        let root_key = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };
        assert_eq!(scene.toplevel_ancestor(root_key), Some(root_key));
        for surface_id in [2, 3] {
            assert_eq!(
                scene.toplevel_ancestor(SurfaceKey {
                    client_id: 1,
                    surface_id
                }),
                Some(root_key),
                "surface {surface_id} composites into the toplevel"
            );
        }
        // An unknown surface, and a committed surface no toplevel reaches,
        // resolve to nothing rather than to an arbitrary window.
        assert_eq!(
            scene.toplevel_ancestor(SurfaceKey {
                client_id: 1,
                surface_id: 99
            }),
            None
        );
        scene.apply(commit(state(1, 4, None))).unwrap();
        assert_eq!(
            scene.toplevel_ancestor(SurfaceKey {
                client_id: 1,
                surface_id: 4
            }),
            None
        );
    }

    fn subsurface(parent: u64) -> Role {
        Role::SubSurface(SubSurfaceState {
            parent: WlSurfaceId(parent),
            location: Point { x: 0, y: 0 },
            sync: true,
        })
    }

    /// `SurfaceNode::parent` is written eagerly by whichever surface last
    /// listed a key as its child, not recomputed on read the way the old
    /// linear scan was -- so removing a child from its only parent's list
    /// must clear the back-pointer, not leave it resolving to a parent that
    /// no longer claims it. (A reparent-and-immediately-relist case would
    /// self-heal even without this, since the add side always overwrites;
    /// orphaning with no new owner is the case that actually needs the
    /// clear.)
    #[test]
    fn removing_a_child_from_its_only_parents_list_orphans_it() {
        let mut scene = Scene::default();
        let a = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };
        let child = SurfaceKey {
            client_id: 1,
            surface_id: 2,
        };

        let mut root_a = state(1, 1, Some(toplevel()));
        root_a.z_ordered_children.push(SubsurfacePosition {
            id: WlSurfaceId(2),
            position: Point { x: 0, y: 0 },
        });
        scene.apply(commit(root_a)).unwrap();
        scene
            .apply(commit(state(1, 2, Some(subsurface(1)))))
            .unwrap();
        assert_eq!(scene.toplevel_ancestor(child), Some(a));

        // A recommits without the child in its list at all.
        scene.apply(commit(state(1, 1, Some(toplevel())))).unwrap();

        assert_eq!(
            scene.toplevel_ancestor(child),
            None,
            "an unlisted child must not keep resolving to its former parent"
        );
    }

    /// The "add" side of `sync_child_back_pointers` overwrites unconditionally,
    /// so a child moving to a new parent resolves to the new one even with
    /// stale prior state present.
    #[test]
    fn reparenting_a_child_updates_its_toplevel_ancestor() {
        let mut scene = Scene::default();
        let b = SurfaceKey {
            client_id: 1,
            surface_id: 10,
        };
        let child = SurfaceKey {
            client_id: 1,
            surface_id: 2,
        };

        let mut root_a = state(1, 1, Some(toplevel()));
        root_a.z_ordered_children.push(SubsurfacePosition {
            id: WlSurfaceId(2),
            position: Point { x: 0, y: 0 },
        });
        scene.apply(commit(root_a)).unwrap();
        scene
            .apply(commit(state(1, 2, Some(subsurface(1)))))
            .unwrap();

        let mut root_b = state(1, 10, Some(toplevel()));
        root_b.z_ordered_children.push(SubsurfacePosition {
            id: WlSurfaceId(2),
            position: Point { x: 0, y: 0 },
        });
        scene.apply(commit(root_b)).unwrap();

        assert_eq!(
            scene.toplevel_ancestor(child),
            Some(b),
            "the child must resolve to its new parent"
        );
    }

    /// A surface's own repaint doesn't re-list it anywhere -- the parent
    /// back-pointer has to survive that surface's later commits on its own,
    /// not just at the moment it was first assigned.
    #[test]
    fn a_childs_own_repaint_preserves_its_parent_without_relisting() {
        let mut scene = Scene::default();
        let root = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };
        let child = SurfaceKey {
            client_id: 1,
            surface_id: 2,
        };

        let mut root_state = state(1, 1, Some(toplevel()));
        root_state.z_ordered_children.push(SubsurfacePosition {
            id: WlSurfaceId(2),
            position: Point { x: 0, y: 0 },
        });
        scene.apply(commit(root_state)).unwrap();
        scene
            .apply(commit(state(1, 2, Some(subsurface(1)))))
            .unwrap();
        assert_eq!(scene.toplevel_ancestor(child), Some(root));

        // The child repaints again; the root never recommits at all.
        scene
            .apply(commit(state(1, 2, Some(subsurface(1)))))
            .unwrap();
        assert_eq!(scene.toplevel_ancestor(child), Some(root));
    }

    /// A parent can list a child that then gets destroyed before ever
    /// committing -- the deferred assignment recorded for it must not
    /// linger forever once that key can never resolve it.
    #[test]
    fn a_child_destroyed_before_its_first_commit_drops_its_pending_assignment() {
        let mut scene = Scene::default();
        let mut root = state(1, 1, Some(toplevel()));
        root.z_ordered_children.push(SubsurfacePosition {
            id: WlSurfaceId(2),
            position: Point { x: 0, y: 0 },
        });
        scene.apply(commit(root)).unwrap();
        assert!(!scene.pending_parents.is_empty());

        scene
            .apply(RecvType::Object(Request::Surface(SurfaceRequest {
                client: ClientId(1),
                surface: WlSurfaceId(2),
                payload: SurfaceRequestPayload::Destroyed,
            })))
            .unwrap();

        assert!(scene.pending_parents.is_empty());
    }

    #[test]
    fn resolves_a_popup_to_the_toplevel_it_hangs_from() {
        let mut scene = Scene::default();
        scene.apply(commit(state(1, 1, Some(toplevel())))).unwrap();
        scene
            .apply(commit(state(
                1,
                2,
                Some(Role::XdgPopup(XdgPopupState {
                    id: XdgPopupId(11),
                    parent_surface_id: WlSurfaceId(1),
                    positioner: XdgPositioner {
                        width: 1,
                        height: 1,
                        anchor_rect: Rectangle::new(0, 0, 1, 1),
                        anchor_edges: 0,
                        gravity: 0,
                        constraint_adjustment: 0,
                        offset: Point { x: 0, y: 0 },
                        reactive: false,
                        parent_size: None,
                        parent_configure: None,
                    },
                    grab_requested: false,
                })),
            )))
            .unwrap();

        assert_eq!(
            scene.toplevel_ancestor(SurfaceKey {
                client_id: 1,
                surface_id: 2
            }),
            Some(SurfaceKey {
                client_id: 1,
                surface_id: 1
            })
        );
    }

    #[test]
    fn destroys_all_client_state_deterministically() {
        let mut scene = Scene::default();
        scene.apply(commit(state(7, 8, Some(toplevel())))).unwrap();
        assert_eq!(scene.surface_count(), 1);
        scene
            .apply(RecvType::Object(Request::ClientDisconnected(ClientId(7))))
            .unwrap();
        assert_eq!(scene.surface_count(), 0);
    }

    #[test]
    fn identity_mismatch_is_rejected() {
        let mut scene = Scene::default();
        scene.apply(RecvType::RawBuffer(vec![0; 4])).unwrap();
        let scene_state = state(1, 2, None);
        let mut scene_state = scene_state;
        scene_state.buffer = Some(external_buffer(1, 1, BufferFormat::Xrgb8888));
        let message = RecvType::Object(Request::Surface(SurfaceRequest {
            client: ClientId(1),
            surface: WlSurfaceId(3),
            payload: SurfaceRequestPayload::Commit(scene_state),
        }));
        assert_eq!(
            scene.apply(message).unwrap_err(),
            SceneError::IdentityMismatch
        );
        assert!(!scene.has_pending_raw_buffer());
    }
}

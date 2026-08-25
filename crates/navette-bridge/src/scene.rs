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
            SurfaceRequestPayload::Destroyed => {
                self.surfaces.remove(&key);
                self.remove_child_references(key);
                Ok(vec![SceneEvent::SurfaceDestroyed(key)])
            }
            SurfaceRequestPayload::Commit(state) => {
                if key != SurfaceKey::new(state.client, state.id) {
                    return Err(SceneError::IdentityMismatch);
                }
                let previous_image = self.surfaces.get(&key).and_then(|node| node.image.clone());
                let image = self.decode_assignment(state.buffer.as_ref(), previous_image)?;
                let node = SurfaceNode {
                    role: role_from_state(&state),
                    children: state
                        .z_ordered_children
                        .iter()
                        .map(|child| Child {
                            key: SurfaceKey::new(state.client, child.id),
                            x: child.position.x,
                            y: child.position.y,
                        })
                        .collect(),
                    image,
                    damage: state.damage.unwrap_or_default(),
                };
                self.surfaces.insert(key, node);
                Ok(vec![SceneEvent::SurfaceCommitted(key)])
            }
        }
    }

    fn apply_toplevel(&mut self, request: ToplevelRequest) -> Result<Vec<SceneEvent>, SceneError> {
        let key = SurfaceKey::new(request.client, request.surface);
        if matches!(request.payload, ToplevelRequestPayload::Destroyed) {
            self.surfaces.remove(&key);
            self.remove_child_references(key);
            return Ok(vec![SceneEvent::SurfaceDestroyed(key)]);
        }
        Ok(Vec::new())
    }

    fn apply_popup(&mut self, request: PopupRequest) -> Result<Vec<SceneEvent>, SceneError> {
        let key = SurfaceKey::new(request.client, request.surface);
        let PopupRequestPayload::Destroyed = request.payload;
        self.surfaces.remove(&key);
        self.remove_child_references(key);
        Ok(vec![SceneEvent::SurfaceDestroyed(key)])
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

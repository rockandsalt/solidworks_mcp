//! Pictures of the part: the add-in's tessellation drawn headless with
//! kiss3d (wgpu), as a grid of orthographic standard views in one PNG.
//!
//! kiss3d keeps its GPU context and resource managers in thread-locals, and
//! its scene graph is `Rc`-based, so all rendering happens on one dedicated
//! thread that lives as long as the process (spawned on first use). Tool
//! calls hand it a [`Model`] and wait for the PNG on a oneshot channel. The
//! GPU device is created once and reused; if creating it fails (no usable
//! adapter) the error is reported and the next call tries again.
//!
//! The device is created here, not by kiss3d (see [`ensure_gpu`]): kiss3d
//! asks wgpu for any backend, wgpu then prefers Vulkan, and some Windows
//! Vulkan drivers crash the whole process while creating the device -- a
//! native access violation that `catch_unwind` can't stop.

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::{Mutex, mpsc};

use anyhow::{Context as _, Result, anyhow};
use image::{ImageBuffer, Rgb, RgbImage};
use kiss3d::camera::{CoordinateSystem2d, FixedView2d, OrbitCamera3d, Projection};
use kiss3d::color::Color;
use kiss3d::glamx::Vec2;
pub use kiss3d::glamx::Vec3;
use kiss3d::post_processing::Tonemap;
use kiss3d::renderer::Polyline3d;
use kiss3d::resource::GpuMesh3d;
use kiss3d::scene::SceneNode3d;
use kiss3d::text::Font;
use kiss3d::window::{CanvasSetup, NumSamples, Window};
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::addin::proto;
use crate::inspect::MM_PER_M;

use rmcp::schemars;

mod section;
pub use section::Plane;
use section::section;

/// A part to draw, in millimeters, model space.
#[derive(Debug, Default, Clone)]
pub struct Model {
    pub faces: Vec<Face>,
    pub edges: Vec<Edge>,
    /// What to frame instead of the whole model ([`Picks::zoom`]).
    pub focus: Option<Aabb>,
}

#[derive(Debug, Default, Clone)]
pub struct Face {
    /// Three vertices per triangle.
    pub vertices: Vec<[f32; 3]>,
    /// One per vertex (outward), or empty to use flat triangle normals.
    pub normals: Vec<[f32; 3]>,
    /// Index into [`HIGHLIGHTS`].
    pub highlight: Option<usize>,
    pub transparent: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Edge {
    pub points: Vec<[f32; 3]>,
    pub highlight: Option<usize>,
    pub transparent: bool,
}

/// An axis-aligned box, mm.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub lo: Vec3,
    pub hi: Vec3,
}

impl Default for Aabb {
    /// Empty: anything added grows it.
    fn default() -> Self {
        Aabb {
            lo: Vec3::splat(f32::INFINITY),
            hi: Vec3::splat(f32::NEG_INFINITY),
        }
    }
}

impl Aabb {
    fn add(&mut self, p: &[f32; 3]) {
        let p = Vec3::from_array(*p);
        self.lo = self.lo.min(p);
        self.hi = self.hi.max(p);
    }

    /// None while empty (or not finite).
    fn valid(self) -> Option<Self> {
        (self.lo.is_finite() && self.hi.is_finite() && self.lo.cmple(self.hi).all()).then_some(self)
    }

    pub fn center(&self) -> Vec3 {
        (self.lo + self.hi) / 2.0
    }
}

/// What render_views' id lists resolved to.
#[derive(Debug, Default, Clone)]
pub struct Picks {
    /// `highlight[i]` drawn in `HIGHLIGHTS[i]`; the first match wins.
    pub highlight: Vec<Target>,
    /// Left out of the picture.
    pub hide: Vec<Target>,
    pub transparent: Vec<Target>,
    /// Everything not highlighted is transparent too.
    pub transparent_rest: bool,
    /// Framed instead of the whole model.
    pub zoom: Vec<Target>,
}

/// How many faces and edges each of [`Picks`]' targets matched.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Matched {
    pub highlight: Vec<usize>,
    pub hide: Vec<usize>,
    pub transparent: Vec<usize>,
    pub zoom: Vec<usize>,
}

/// Counts every target that `hit`s; true if any did.
fn count_hits(targets: &[Target], hit: &dyn Fn(&Target) -> bool, counts: &mut [usize]) -> bool {
    let mut any = false;
    for (target, count) in targets.iter().zip(counts) {
        if hit(target) {
            *count += 1;
            any = true;
        }
    }
    any
}

/// Something to color in: what a highlight id or feature name resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Face(Vec<u8>),
    Edge(Vec<u8>),
    /// Every face and edge of the body.
    Body(Vec<u8>),
    /// The faces the feature created.
    Feature(Vec<u8>),
    FeatureName(String),
    /// Every face and edge of an assembly component.
    Component(Vec<u8>),
}

impl Target {
    fn face(&self, face: &proto::FaceMesh) -> bool {
        match self {
            Target::Face(id) => face.persist_id == *id,
            Target::Body(id) => face.body_persist_id == *id,
            Target::Feature(id) => face.feature_persist_id == *id,
            Target::FeatureName(name) => face.feature_name == *name,
            Target::Component(id) => face.component_persist_id == *id,
            Target::Edge(_) => false,
        }
    }

    fn edge(&self, edge: &proto::EdgePolyline) -> bool {
        match self {
            Target::Edge(id) => edge.persist_id == *id,
            Target::Body(id) => edge.body_persist_id == *id,
            Target::Component(id) => edge.component_persist_id == *id,
            _ => false,
        }
    }
}

/// Converts the add-in's mesh (meters, assembly components already placed by
/// crate::assembly::place_mesh) to a [`Model`] (millimeters), applying
/// `picks`. Hidden entities still count for zoom.
pub fn model_from_mesh(mesh: &proto::GetMeshResponse, picks: &Picks) -> (Model, Matched) {
    let mut matched = Matched {
        highlight: vec![0; picks.highlight.len()],
        hide: vec![0; picks.hide.len()],
        transparent: vec![0; picks.transparent.len()],
        zoom: vec![0; picks.zoom.len()],
    };
    let mut focus = Aabb::default();
    let highlights = &picks.highlight[..picks.highlight.len().min(HIGHLIGHTS.len())];
    // (highlight, transparent), or None to leave it out.
    let mut pick = |hit: &dyn Fn(&Target) -> bool, points: &[[f32; 3]]| {
        if count_hits(&picks.zoom, hit, &mut matched.zoom) {
            points.iter().for_each(|p| focus.add(p));
        }
        if count_hits(&picks.hide, hit, &mut matched.hide) {
            return None;
        }
        let highlight = highlights.iter().position(hit);
        if let Some(i) = highlight {
            matched.highlight[i] += 1;
        }
        let transparent = count_hits(&picks.transparent, hit, &mut matched.transparent)
            || (picks.transparent_rest && highlight.is_none());
        Some((highlight, transparent))
    };
    let points = |v: &[f32]| -> Vec<[f32; 3]> {
        v.chunks_exact(3)
            .map(|p| {
                [
                    p[0] * MM_PER_M as f32,
                    p[1] * MM_PER_M as f32,
                    p[2] * MM_PER_M as f32,
                ]
            })
            .collect()
    };
    let directions =
        |v: &[f32]| -> Vec<[f32; 3]> { v.chunks_exact(3).map(|p| [p[0], p[1], p[2]]).collect() };

    let mut faces = Vec::new();
    for f in &mesh.faces {
        let vertices = points(&f.triangles);
        let Some((highlight, transparent)) = pick(&|t| t.face(f), &vertices) else { continue };
        faces.push(Face {
            vertices,
            normals: directions(&f.normals),
            highlight,
            transparent,
        });
    }
    let mut edges = Vec::new();
    for e in &mesh.edges {
        let points = points(&e.points);
        let Some((highlight, transparent)) = pick(&|t| t.edge(e), &points) else { continue };
        edges.push(Edge {
            points,
            highlight,
            transparent,
        });
    }
    let focus = focus.valid();
    (Model { faces, edges, focus }, matched)
}

impl Model {
    pub fn triangle_count(&self) -> usize {
        self.faces.iter().map(|f| f.vertices.len() / 3).sum()
    }

    /// The box around every face and edge; None if there's nothing.
    pub fn bounding_box(&self) -> Option<Aabb> {
        let mut aabb = Aabb::default();
        self.faces
            .iter()
            .flat_map(|f| f.vertices.iter())
            .chain(self.edges.iter().flat_map(|e| e.points.iter()))
            .for_each(|p| aabb.add(p));
        aabb.valid()
    }
}

/// Colors for highlighted entities, in order of the ids asked for. Picked
/// to stand out from the gray part and from each other.
pub const HIGHLIGHTS: [(&str, [f32; 3]); 6] = [
    ("orange", [1.0, 0.45, 0.0]),
    ("magenta", [0.9, 0.1, 0.75]),
    ("green", [0.1, 0.75, 0.2]),
    ("cyan", [0.0, 0.75, 0.9]),
    ("red", [0.9, 0.1, 0.1]),
    ("yellow", [0.95, 0.85, 0.0]),
];

const PART_COLOR: [f32; 3] = [0.62, 0.66, 0.72];
const BACKGROUND: [f32; 3] = [1.0, 1.0, 1.0];
const EDGE_COLOR: [f32; 3] = [0.08, 0.08, 0.1];
/// Where a section cuts through material (highlighted material: its color, darker).
const CUT_COLOR: [f32; 3] = [0.8, 0.64, 0.4];
const TRANSPARENT_ALPHA: f32 = 0.25;
const TRANSPARENT_EDGE_COLOR: [f32; 3] = [0.6, 0.62, 0.67];

/// A standard view direction, named like SolidWorks' standard views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum View {
    Front,
    Back,
    Left,
    Right,
    Top,
    Bottom,
    /// From +X +Y +Z, like SolidWorks' isometric view.
    Isometric,
    /// From +X -Y +Z: the isometric view seen from underneath.
    IsometricBelow,
}

impl View {
    /// The default layout: third-angle projection (top above front, right
    /// beside front) with the isometric view in the free corner.
    pub const DEFAULT_GRID: [View; 4] = [View::Top, View::Isometric, View::Front, View::Right];

    pub fn label(self) -> &'static str {
        match self {
            View::Front => "Front",
            View::Back => "Back",
            View::Left => "Left",
            View::Right => "Right",
            View::Top => "Top",
            View::Bottom => "Bottom",
            View::Isometric => "Isometric",
            View::IsometricBelow => "Isometric (below)",
        }
    }

    /// (direction from the part toward the eye, screen-up direction).
    fn orientation(self) -> (Vec3, Vec3) {
        match self {
            View::Front => (Vec3::Z, Vec3::Y),
            View::Back => (Vec3::NEG_Z, Vec3::Y),
            View::Right => (Vec3::X, Vec3::Y),
            View::Left => (Vec3::NEG_X, Vec3::Y),
            View::Top => (Vec3::Y, Vec3::NEG_Z),
            View::Bottom => (Vec3::NEG_Y, Vec3::Z),
            View::Isometric => (Vec3::ONE.normalize(), Vec3::Y),
            View::IsometricBelow => (Vec3::new(1.0, -1.0, 1.0).normalize(), Vec3::Y),
        }
    }

}

/// A view: a standard view's name, or any direction.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ViewSpec {
    Named(View),
    Direction {
        /// From the part toward the eye, model axes, e.g. [1, 1, 0].
        direction: [f32; 3],
        /// The model direction that points up on screen (made perpendicular to `direction`).
        /// Defaults to +Y, or to -Z / +Z when looking along Y.
        #[serde(default)]
        up: Option<[f32; 3]>,
    },
}

/// Where a view looks from: unit (right, up, toward-eye) screen axes in
/// model space.
#[derive(Debug, Clone, PartialEq)]
pub struct Camera {
    pub label: String,
    right: Vec3,
    up: Vec3,
    toward_eye: Vec3,
}

impl Camera {
    pub fn standard(view: View) -> Self {
        let (toward_eye, up) = view.orientation();
        Self::new(view.label().to_string(), toward_eye, up)
    }

    /// `up_hint` must not be parallel to `toward_eye`; both unit length.
    fn new(label: String, toward_eye: Vec3, up_hint: Vec3) -> Self {
        let right = up_hint.cross(toward_eye).normalize();
        let up = toward_eye.cross(right);
        Camera {
            label,
            right,
            up,
            toward_eye,
        }
    }

    pub fn from_spec(spec: ViewSpec) -> Result<Self, String> {
        let (direction, up) = match spec {
            ViewSpec::Named(view) => return Ok(Self::standard(view)),
            ViewSpec::Direction { direction, up } => (Vec3::from_array(direction), up.map(Vec3::from_array)),
        };
        let toward_eye = direction
            .try_normalize()
            .ok_or_else(|| format!("view direction {direction} has no length"))?;
        let up = match up {
            Some(up) => up,
            None if toward_eye.y.abs() > 0.99 => Vec3::new(0.0, 0.0, -toward_eye.y.signum()),
            None => Vec3::Y,
        };
        let up = up
            .reject_from(toward_eye)
            .try_normalize()
            .filter(|_| up.normalize_or_zero().dot(toward_eye).abs() < 0.999)
            .ok_or_else(|| format!("view up {up} is (nearly) along the view direction"))?;
        let d = toward_eye;
        let label = format!("From ({:.2}, {:.2}, {:.2})", d.x, d.y, d.z).replace("-0.00", "0.00");
        Ok(Self::new(label, toward_eye, up))
    }

    /// Looking straight at a section's cut, and at it from an angle.
    pub fn section_views(normal: Vec3) -> [Camera; 2] {
        let normal = normal.normalize();
        // Tilt toward the isometric direction, or X if that's the normal.
        let tilt = Vec3::ONE
            .reject_from(normal)
            .try_normalize()
            .unwrap_or_else(|| Vec3::X.reject_from(normal).normalize());
        let spec = |d: Vec3| {
            Self::from_spec(ViewSpec::Direction {
                direction: d.to_array(),
                up: None,
            })
            .expect("unit directions, default up")
        };
        let mut straight = spec(normal);
        straight.label = "Section".into();
        let mut oblique = spec(normal + tilt * 0.8);
        oblique.label = "Section (angled)".into();
        [straight, oblique]
    }

    /// Which model directions point right and up on screen, e.g.
    /// "+X right, -Z up", when both are model axes; None otherwise.
    pub fn screen_axes(&self) -> Option<String> {
        Some(format!(
            "{} right, {} up",
            axis_name(self.right)?,
            axis_name(self.up)?
        ))
    }
}

/// "+X", "-Z", ... for a unit vector along a model axis.
fn axis_name(v: Vec3) -> Option<&'static str> {
    const AXES: [(Vec3, &str); 6] = [
        (Vec3::X, "+X"),
        (Vec3::NEG_X, "-X"),
        (Vec3::Y, "+Y"),
        (Vec3::NEG_Y, "-Y"),
        (Vec3::Z, "+Z"),
        (Vec3::NEG_Z, "-Z"),
    ];
    AXES.iter()
        .find(|(a, _)| a.dot(v) > 0.999)
        .map(|(_, name)| *name)
}

#[derive(Debug, Clone)]
pub struct Request {
    pub model: Model,
    /// Drawn row by row, `columns` per row.
    pub views: Vec<Camera>,
    pub columns: usize,
    /// Side of each (square) tile, pixels.
    pub tile_px: u32,
    /// Must keep some of `model` ([`Plane::keeps_any`]).
    pub section: Option<Plane>,
}

#[derive(Debug)]
pub struct Rendered {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// How many millimeters of the model one tile spans, edge to edge
    /// (every view uses the same scale).
    pub tile_span_mm: f32,
}

type Job = (Request, oneshot::Sender<Result<Rendered>>);

/// Handle to the render thread. Cheap to share; the thread starts on the
/// first render.
#[derive(Default)]
pub struct Renderer {
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
}

impl Renderer {
    pub async fn render(&self, request: Request) -> Result<Rendered> {
        let (reply, result) = oneshot::channel();
        {
            let mut jobs = self.jobs.lock().unwrap_or_else(|p| p.into_inner());
            // Panics are caught on the thread, so it only goes away if it
            // never started; (re)start it then.
            let unsent = match jobs.as_ref() {
                Some(tx) => tx
                    .send((request, reply))
                    .err()
                    .map(|mpsc::SendError(job)| job),
                None => Some((request, reply)),
            };
            if let Some(job) = unsent {
                let (tx, rx) = mpsc::channel::<Job>();
                std::thread::Builder::new()
                    .name("render".into())
                    .spawn(move || render_thread(rx))
                    .context("couldn't start the render thread")?;
                tx.send(job)
                    .map_err(|_| anyhow!("the render thread exited"))?;
                *jobs = Some(tx);
            }
        }
        result
            .await
            .map_err(|_| anyhow!("the render thread dropped the request"))?
    }
}

fn render_thread(jobs: mpsc::Receiver<Job>) {
    let mut window: Option<Window> = None;
    for (request, reply) in jobs {
        let result = catch_unwind(AssertUnwindSafe(|| render_job(&mut window, &request)))
            .unwrap_or_else(|panic| {
                // Start from a fresh window next time; it may be half-broken.
                window = None;
                let what = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown error");
                Err(anyhow!("the renderer failed: {what}"))
            });
        let _ = reply.send(result);
    }
}

fn color(c: [f32; 3]) -> Color {
    Color::new(c[0], c[1], c[2], 1.0)
}

/// Center and radius of a box's bounding sphere.
fn sphere(aabb: Aabb) -> (Vec3, f32) {
    let radius = (aabb.hi - aabb.lo).length() / 2.0;
    (aabb.center(), if radius > 0.0 { radius } else { 1.0 })
}

/// Backends to try, in order, each on its own; `WGPU_BACKEND` (e.g. "dx12",
/// "vulkan", "gl") replaces the list.
///
/// On Windows that's DX12 only. wgpu's default choice there is Vulkan, and
/// the Vulkan driver of an Intel UHD 630 (driver 27.20.100.8729) crashes in
/// vkCreateDevice with an access violation, taking the MCP server down with
/// it; the same GPU renders fine through DX12. GL renders too but crashes
/// when the context is torn down.
fn backend_candidates() -> Vec<wgpu::Backends> {
    if let Some(backends) = wgpu::Backends::from_env() {
        return vec![backends];
    }
    if cfg!(windows) {
        vec![wgpu::Backends::DX12]
    } else {
        vec![wgpu::Backends::PRIMARY, wgpu::Backends::GL]
    }
}

/// Makes sure kiss3d's (thread-local) wgpu context exists, creating the
/// device ourselves. kiss3d reuses an existing context when it opens a
/// window, so its own adapter selection never runs. Called before every
/// window is created: kiss3d drops the context when its last window goes
/// away (e.g. after a panic).
fn ensure_gpu() -> Result<()> {
    if kiss3d::context::Context::is_initialized() {
        return Ok(());
    }
    let mut tried = Vec::new();
    for backends in backend_candidates() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            // No validation/debug layers, even in debug builds; WGPU_VALIDATION
            // and WGPU_DEBUG turn them back on.
            flags: wgpu::InstanceFlags::empty().with_env(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        // A real GPU first; then the software fallback (WARP on Windows).
        let adapter = [false, true].into_iter().find_map(|fallback| {
            kiss3d::pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::from_env().unwrap_or_default(),
                compatible_surface: None,
                force_fallback_adapter: fallback,
                apply_limit_buckets: false,
            }))
            .ok()
        });
        let Some(adapter) = adapter else {
            tried.push(format!("{backends:?}: no adapter"));
            continue;
        };
        let info = adapter.get_info();
        // Only what kiss3d's rasterizer uses: GPU timestamps if available, and
        // the adapter's full limits (its shadow/point/wireframe renderers need
        // more bind groups and storage buffers than wgpu's defaults allow). Not
        // kiss3d's experimental ray queries -- the path tracer isn't used here.
        let device = kiss3d::pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("render_views"),
            required_features: adapter.features() & wgpu::Features::TIMESTAMP_QUERY,
            required_limits: adapter.limits(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        }));
        match device {
            Ok((device, queue)) => {
                tracing::info!(
                    backend = ?info.backend,
                    adapter = %info.name,
                    driver = %info.driver_info,
                    "render_views: using GPU adapter"
                );
                kiss3d::context::Context::init(
                    instance,
                    device,
                    queue,
                    adapter,
                    // What kiss3d picks for headless windows; gamma is done
                    // in its shaders.
                    wgpu::TextureFormat::Rgba8Unorm,
                );
                return Ok(());
            }
            Err(err) => tried.push(format!("{:?} {}: {err}", info.backend, info.name)),
        }
    }
    Err(anyhow!("no usable GPU adapter ({})", tried.join("; ")))
}

fn render_job(window: &mut Option<Window>, request: &Request) -> Result<Rendered> {
    let sectioned;
    let (model, outline, caps) = match &request.section {
        Some(plane) => {
            sectioned = section(&request.model, plane);
            (&sectioned.model, sectioned.outline.as_slice(), sectioned.caps.as_slice())
        }
        None => (&request.model, &[][..], &[][..]),
    };
    // The caller refuses an empty model and a section that keeps nothing
    // (Plane::keeps_any) with a message of its own; this is the backstop.
    let bounds = model.bounding_box().filter(|_| !model.faces.is_empty());
    let (center, radius) = sphere(bounds.ok_or_else(|| anyhow!("the part has no geometry to draw"))?);
    // Everything is drawn in a normalized space: centered, bounding radius
    // 1. Keeps camera distances and clip planes independent of part size.
    let to_scene = |p: &[f32; 3]| (Vec3::from_array(*p) - center) / radius;
    // What the tiles frame, in that space, with a 10% margin: the model's
    // bounding sphere (radius 1), or the zoom box's.
    let (frame_center, half_span) = match model.focus {
        Some(focus) => {
            let (c, r) = sphere(focus);
            ((c - center) / radius, (r / radius).max(1e-3) * 1.1)
        }
        None => (Vec3::ZERO, 1.1),
    };

    let tile = request.tile_px;
    if window
        .as_ref()
        .is_none_or(|w| w.size().x != tile || w.size().y != tile)
    {
        ensure_gpu()?;
        let setup = CanvasSetup {
            samples: NumSamples::Four,
            ..Default::default()
        };
        // Create the new window before dropping the old one, so kiss3d keeps
        // (rather than tears down and rebuilds) the GPU device.
        let mut new =
            kiss3d::pollster::block_on(Window::new_headless_with_setup(tile, tile, setup));
        new.set_background_color(color(BACKGROUND));
        new.set_tonemap(Tonemap::None);
        new.set_ambient(0.35);
        // Cast shadows read as extra faces in a technical view.
        new.set_shadows_enabled(false);
        *window = Some(new);
    }
    let window = window.as_mut().unwrap();

    let mut scene = SceneNode3d::empty();
    // One mesh per look: the plain part, then each highlight, each opaque
    // and transparent; then the section's cut faces.
    for transparent in [false, true] {
        for group in std::iter::once(None).chain((0..HIGHLIGHTS.len()).map(Some)) {
            let faces: Vec<&Face> = model
                .faces
                .iter()
                .filter(|f| f.highlight == group && f.transparent == transparent)
                .collect();
            if faces.is_empty() {
                continue;
            }
            let rgb = group.map_or(PART_COLOR, |i| HIGHLIGHTS[i].1);
            let alpha = if transparent { TRANSPARENT_ALPHA } else { 1.0 };
            add_mesh(&mut scene, &faces, &to_scene, Color::new(rgb[0], rgb[1], rgb[2], alpha));
        }
    }
    for cap in caps {
        let rgb = cap.highlight.map_or(CUT_COLOR, |i| HIGHLIGHTS[i].1.map(|c| c * 0.7));
        add_mesh(&mut scene, &[cap], &to_scene, color(rgb));
    }

    let mut edges: Vec<Polyline3d> = model
        .edges
        .iter()
        .filter(|e| e.points.len() >= 2)
        .map(|e| {
            let line = Polyline3d::new(e.points.iter().map(to_scene).collect());
            match (e.highlight, e.transparent) {
                // Pulled slightly toward the viewer so they win over the faces
                // they bound (kiss3d: positive bias = nearer, as a fraction of
                // depth). Highlighted edges are thicker and pulled further.
                (Some(i), _) => line
                    .with_color(color(HIGHLIGHTS[i].1))
                    .with_width(5.0)
                    .with_depth_bias(0.006),
                (None, false) => line
                    .with_color(color(EDGE_COLOR))
                    .with_width(1.5)
                    .with_depth_bias(0.003),
                (None, true) => line
                    .with_color(color(TRANSPARENT_EDGE_COLOR))
                    .with_width(1.0)
                    .with_depth_bias(0.003),
            }
        })
        .collect();
    // The cut's outline, where faces cross the section plane.
    edges.extend(outline.iter().filter(|l| l.len() >= 2).map(|l| {
        Polyline3d::new(l.iter().map(to_scene).collect())
            .with_color(color(EDGE_COLOR))
            .with_width(2.0)
            .with_depth_bias(0.004)
    }));

    let font = Font::default();
    let columns = request.columns.max(1);
    let rows = request.views.len().div_ceil(columns);
    let (width, height) = (tile * columns as u32, tile * rows as u32);
    let mut out: RgbImage = ImageBuffer::from_pixel(width, height, Rgb([255, 255, 255]));

    // Orthographic: the frame is set by distance * tan(fov / 2). The eye is
    // far enough out that all of the model (radius 1 around the origin) is
    // in front of it and within the far plane.
    let dist = 4.0 + frame_center.length();
    let fov = 2.0 * (half_span / dist).atan();

    for (i, view) in request.views.iter().enumerate() {
        let (right, up, toward_eye) = (view.right, view.up, view.toward_eye);
        let eye = frame_center + toward_eye * dist;
        let mut camera = OrbitCamera3d::new_with_frustum(fov, 0.1, 2.0 * dist, eye, frame_center);
        camera.set_up_axis_dir(up);
        camera.look_at(eye, frame_center);
        camera.set_projection(Projection::Orthographic);

        // Key light from the upper left, a little behind the viewer, so faces
        // at different angles get different shades.
        let mut light =
            scene.add_directional_light(-(toward_eye * 1.0 + up * 0.6 - right * 0.4).normalize());

        for edge in &edges {
            window.draw_polyline(edge);
        }
        let px = tile as f32;
        window.draw_text(
            &view.label,
            Vec2::new(px * 0.03, px * 0.02),
            px * 0.06,
            &font,
            color([0.2, 0.2, 0.25]),
        );
        draw_triad(window, &font, view, px);

        let mut camera_2d = FixedView2d::new(CoordinateSystem2d::TopLeftDown, false);
        kiss3d::pollster::block_on(window.render(
            Some(&mut scene),
            None,
            Some(&mut camera),
            Some(&mut camera_2d),
            None,
            None,
        ));
        let shot = window.snap_image();
        light.remove();

        let (x, y) = ((i % columns) as u32 * tile, (i / columns) as u32 * tile);
        image::imageops::replace(&mut out, &shot, x.into(), y.into());
    }

    // Thin separators between tiles.
    for c in 1..columns as u32 {
        for y in 0..height {
            out.put_pixel(c * tile, y, Rgb([200, 200, 205]));
        }
    }
    for r in 1..rows as u32 {
        for x in 0..width {
            out.put_pixel(x, r * tile, Rgb([200, 200, 205]));
        }
    }

    let mut png = Vec::new();
    out.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("couldn't encode the PNG")?;
    Ok(Rendered {
        png,
        width,
        height,
        tile_span_mm: 2.0 * half_span * radius,
    })
}

/// Adds the faces as one mesh, in scene space (via `to_scene`), drawn from
/// both sides: SolidWorks' winding isn't guaranteed.
fn add_mesh<F: std::borrow::Borrow<Face>>(
    scene: &mut SceneNode3d,
    faces: &[F],
    to_scene: &dyn Fn(&[f32; 3]) -> Vec3,
    color: Color,
) {
    let mut coords = Vec::new();
    let mut normals = Vec::new();
    for face in faces {
        let face = face.borrow();
        let n = face.vertices.len() / 3 * 3;
        coords.extend(face.vertices[..n].iter().map(to_scene));
        if face.normals.len() == face.vertices.len() {
            normals.extend(face.normals[..n].iter().map(|v| Vec3::from_array(*v).normalize_or_zero()));
        } else {
            for t in face.vertices[..n].chunks_exact(3) {
                let [a, b, c] = [0, 1, 2].map(|i| Vec3::from_array(t[i]));
                normals.extend([(b - a).cross(c - a).normalize_or_zero(); 3]);
            }
        }
    }
    let indices = (0..coords.len() as u32 / 3)
        .map(|i| [3 * i, 3 * i + 1, 3 * i + 2])
        .collect();
    let mesh = GpuMesh3d::new(coords, indices, Some(normals), None, false);
    scene
        .add_mesh(Rc::new(RefCell::new(mesh)), Vec3::ONE)
        .set_color(color)
        .set_metallic(0.0)
        .set_roughness(0.8)
        .enable_backface_culling(false);
}

/// Model axes in the bottom-left corner: X red, Y green, Z blue, as in
/// SolidWorks. Axes pointing (nearly) at the viewer are drawn as a dot.
fn draw_triad(window: &mut Window, font: &std::sync::Arc<Font>, view: &Camera, px: f32) {
    let (right, up) = (view.right, view.up);
    let origin = Vec2::new(px * 0.12, px * 0.86);
    let len = px * 0.06;
    for (axis, name, rgb) in [
        (Vec3::X, "X", [0.85, 0.1, 0.1]),
        (Vec3::Y, "Y", [0.1, 0.6, 0.1]),
        (Vec3::Z, "Z", [0.1, 0.2, 0.9]),
    ] {
        // Screen y grows downward.
        let d = Vec2::new(axis.dot(right), -axis.dot(up));
        let tip = origin + d * len;
        if d.length() > 0.2 {
            window.draw_line_2d(origin, tip, color(rgb), 2.5);
        } else {
            window.draw_line_2d(
                origin - Vec2::splat(2.0),
                origin + Vec2::splat(2.0),
                color(rgb),
                4.0,
            );
        }
        let label = origin + d.normalize_or(Vec2::new(0.7, -0.7)) * (len + px * 0.025)
            - Vec2::new(px * 0.012, px * 0.02);
        window.draw_text(name, label, px * 0.04, font, color(rgb));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_converts_to_mm_and_applies_picks() {
        let face = |id: &[u8], body: &[u8], feature: &str| proto::FaceMesh {
            persist_id: id.to_vec(),
            body_persist_id: body.to_vec(),
            feature_name: feature.into(),
            triangles: vec![0.0, 0.0, 0.0, 0.01, 0.0, 0.0, 0.0, 0.02, 0.0],
            normals: vec![0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0],
            ..Default::default()
        };
        let mesh = proto::GetMeshResponse {
            faces: vec![
                face(b"f1", b"b1", "Boss-Extrude1"),
                face(b"f2", b"b1", "Cut-Extrude1"),
                face(b"f3", b"b2", "Boss-Extrude2"),
            ],
            edges: vec![proto::EdgePolyline {
                persist_id: b"e1".to_vec(),
                body_persist_id: b"b1".to_vec(),
                points: vec![0.0, 0.0, 0.0, 0.001, 0.0, 0.0],
                ..Default::default()
            }],
            ..Default::default()
        };
        let picks = Picks {
            highlight: vec![
                Target::Face(b"f2".to_vec()),
                Target::FeatureName("Cut-Extrude1".into()),
                Target::Body(b"b1".to_vec()),
                Target::Edge(b"nope".to_vec()),
            ],
            hide: vec![Target::Body(b"b2".to_vec())],
            transparent: vec![Target::Face(b"f1".to_vec())],
            transparent_rest: false,
            zoom: vec![Target::Face(b"f3".to_vec()), Target::Edge(b"e1".to_vec())],
        };
        let (model, matched) = model_from_mesh(&mesh, &picks);
        assert_eq!(model.faces.len(), 2, "body b2 is hidden");
        assert_eq!(model.faces[0].vertices[1], [10.0, 0.0, 0.0]);
        assert_eq!(model.faces[0].normals[0], [0.0, 0.0, 1.0]);
        assert_eq!(model.edges[0].points[1], [1.0, 0.0, 0.0]);
        // f1 only matches the body; f2 matches the face first.
        assert_eq!(model.faces[0].highlight, Some(2));
        assert_eq!(model.faces[1].highlight, Some(0));
        assert_eq!(model.edges[0].highlight, Some(2));
        assert!(model.faces[0].transparent && !model.faces[1].transparent);
        assert_eq!(
            matched,
            Matched {
                highlight: vec![1, 0, 2, 0],
                hide: vec![1],
                transparent: vec![1],
                zoom: vec![1, 1],
            }
        );
        // The hidden face still counts for zoom.
        let focus = model.focus.unwrap();
        assert_eq!((focus.lo, focus.hi), (Vec3::ZERO, Vec3::new(10.0, 20.0, 0.0)));
        assert_eq!(model.triangle_count(), 2);

        let rest = Picks {
            highlight: vec![Target::Face(b"f2".to_vec())],
            transparent_rest: true,
            ..Default::default()
        };
        let (model, _) = model_from_mesh(&mesh, &rest);
        let transparent: Vec<bool> = model.faces.iter().map(|f| f.transparent).collect();
        assert_eq!(transparent, [true, false, true]);
        assert!(model.edges[0].transparent && model.focus.is_none());
    }

    #[test]
    fn screen_axes_match_solidworks_views() {
        let axes = |v| Camera::standard(v).screen_axes();
        assert_eq!(axes(View::Front).unwrap(), "+X right, +Y up");
        assert_eq!(axes(View::Top).unwrap(), "+X right, -Z up");
        assert_eq!(axes(View::Right).unwrap(), "-Z right, +Y up");
        assert_eq!(axes(View::Back).unwrap(), "-X right, +Y up");
        assert_eq!(axes(View::Left).unwrap(), "+Z right, +Y up");
        assert_eq!(axes(View::Bottom).unwrap(), "+X right, +Z up");
        assert!(axes(View::Isometric).is_none());
    }

    #[test]
    fn views_from_any_direction() {
        let from = |direction: [f32; 3], up: Option<[f32; 3]>| {
            Camera::from_spec(ViewSpec::Direction { direction, up })
        };
        // Along an axis, the same as the standard views.
        let axes = |c: Result<Camera, String>| c.unwrap().screen_axes();
        assert_eq!(axes(from([0.0, 0.0, 2.0], None)), Camera::standard(View::Front).screen_axes());
        assert_eq!(axes(from([0.0, 1.0, 0.0], None)), Camera::standard(View::Top).screen_axes());
        assert_eq!(axes(from([0.0, -1.0, 0.0], None)), Camera::standard(View::Bottom).screen_axes());
        let tilted = from([1.0, 0.0, 1.0], Some([0.0, 0.0, 1.0])).unwrap();
        assert_eq!(tilted.label, "From (0.71, 0.00, 0.71)");
        assert!(tilted.up.dot(tilted.toward_eye).abs() < 1e-6 && tilted.up.z > 0.0);
        assert!(from([0.0; 3], None).is_err());
        assert!(from([1.0, 0.0, 0.0], Some([2.0, 0.0, 0.0])).is_err());

        let named: ViewSpec = serde_json::from_value(serde_json::json!("isometric")).unwrap();
        assert_eq!(named, ViewSpec::Named(View::Isometric));
        let free: ViewSpec = serde_json::from_value(serde_json::json!({"direction": [1, 2, 3]})).unwrap();
        assert_eq!(free, ViewSpec::Direction { direction: [1.0, 2.0, 3.0], up: None });

        let [straight, oblique] = Camera::section_views(Vec3::X);
        assert_eq!(straight.toward_eye, Vec3::X);
        assert!(oblique.toward_eye.x > 0.5 && oblique.toward_eye.x < 0.99);
    }

    /// A closed cylinder along +Y: radius `r` around (x, z), from y0 to y1.
    pub(super) fn cylinder(x: f32, z: f32, r: f32, y0: f32, y1: f32, highlight: Option<usize>) -> Vec<Face> {
        let n = 48;
        let at = |i: usize, y: f32| {
            let t = (i as f32 + 0.5) / n as f32 * std::f32::consts::TAU;
            [x + r * t.cos(), y, z + r * t.sin()]
        };
        let out = |p: [f32; 3]| [(p[0] - x) / r, 0.0, (p[2] - z) / r];
        let mut side = Face { highlight, ..Default::default() };
        let mut caps = Face { highlight, ..Default::default() };
        for i in 0..n {
            let [a, b, c, d] = [at(i, y0), at(i + 1, y0), at(i + 1, y1), at(i, y1)];
            side.vertices.extend([a, b, c, a, c, d]);
            side.normals.extend([a, b, c, a, c, d].map(out));
            caps.vertices.extend([[x, y0, z], a, b, [x, y1, z], c, d]);
            caps.normals.extend([[0.0, -1.0, 0.0]; 3]);
            caps.normals.extend([[0.0, 1.0, 0.0]; 3]);
        }
        vec![side, caps]
    }

    /// A quad a-b-c-d (counter-clockwise seen from outside) and its outline.
    fn quad(model: &mut Model, q: [[f32; 3]; 4], highlight: Option<usize>) {
        let [a, b, c, d] = q;
        model.faces.push(Face {
            vertices: vec![a, b, c, a, c, d],
            normals: vec![],
            highlight,
            transparent: false,
        });
        for (p, q) in [(a, b), (b, c), (c, d), (d, a)] {
            model.edges.push(Edge {
                points: vec![p, q],
                ..Default::default()
            });
        }
    }

    /// An L-shaped bracket (60 x 40 x 30 mm) with a round boss on its foot.
    pub(super) fn bracket(highlight_boss: bool) -> Model {
        let mut m = Model::default();
        // L profile in XY, extruded 40 along Z: foot 60 x 10, wall 10 x 30.
        let profile: [[f32; 2]; 6] = [
            [0., 0.],
            [60., 0.],
            [60., 10.],
            [10., 10.],
            [10., 30.],
            [0., 30.],
        ];
        let (z0, z1) = (0.0, 40.0);
        for i in 0..6 {
            let (p, q) = (profile[i], profile[(i + 1) % 6]);
            quad(
                &mut m,
                [
                    [p[0], p[1], z0],
                    [q[0], q[1], z0],
                    [q[0], q[1], z1],
                    [p[0], p[1], z1],
                ],
                None,
            );
        }
        for (z, flip) in [(z1, false), (z0, true)] {
            for rect in [[[0., 0.], [60., 10.]], [[0., 10.], [10., 30.]]] {
                let [lo, hi] = rect;
                let mut q = [
                    [lo[0], lo[1], z],
                    [hi[0], lo[1], z],
                    [hi[0], hi[1], z],
                    [lo[0], hi[1], z],
                ];
                if flip {
                    q.reverse();
                }
                m.faces.push(Face {
                    vertices: vec![q[0], q[1], q[2], q[0], q[2], q[3]],
                    ..Default::default()
                });
            }
        }
        // Boss: radius 6, from the foot's top (y = 10) up to y = 18, at x = 40, z = 20.
        let (cx, cz, r, y0, y1) = (40.0f32, 20.0f32, 6.0f32, 10.0f32, 18.0f32);
        let n = 48;
        let at = |i: usize, y: f32| {
            let t = i as f32 / n as f32 * std::f32::consts::TAU;
            [cx + r * t.cos(), y, cz + r * t.sin()]
        };
        let mut side = Face {
            highlight: highlight_boss.then_some(0),
            ..Default::default()
        };
        let mut cap = Face {
            highlight: None,
            ..Default::default()
        };
        for i in 0..n {
            let (a, b) = (at(i, y0), at(i + 1, y0));
            let (c, d) = (at(i + 1, y1), at(i, y1));
            let na = [(a[0] - cx) / r, 0.0, (a[2] - cz) / r];
            let nb = [(b[0] - cx) / r, 0.0, (b[2] - cz) / r];
            side.vertices.extend([a, c, b, a, d, c]);
            side.normals.extend([na, nb, nb, na, na, nb]);
            cap.vertices.extend([[cx, y1, cz], c, d]);
        }
        m.faces.push(side);
        m.faces.push(cap);
        for y in [y0, y1] {
            m.edges.push(Edge {
                points: (0..=n).map(|i| at(i, y)).collect(),
                highlight: highlight_boss.then_some(1),
                ..Default::default()
            });
        }
        m
    }

    /// Needs a GPU adapter (or lavapipe/WARP); writes PNGs for eyeballing.
    #[test]
    #[ignore = "needs a GPU adapter"]
    fn renders_a_bracket() {
        let renderer = Renderer::default();
        let out_dir = std::env::var("RENDER_TEST_OUT").unwrap_or_else(|_| ".".into());
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let standard = |views: &[View]| views.iter().map(|v| Camera::standard(*v)).collect::<Vec<_>>();
        let whole_span = 2.2 * 0.5 * (60f32 * 60. + 30. * 30. + 40. * 40.).sqrt();
        let request = |model, views: Vec<Camera>, columns, tile_px| Request {
            model,
            views,
            columns,
            tile_px,
            section: None,
        };

        let mut transparent = bracket(true);
        for face in &mut transparent.faces {
            face.transparent = face.highlight.is_none();
        }
        for edge in &mut transparent.edges {
            edge.transparent = edge.highlight.is_none();
        }
        let mut zoomed = bracket(true);
        zoomed.focus = Some(Aabb {
            lo: Vec3::new(34.0, 10.0, 14.0),
            hi: Vec3::new(46.0, 18.0, 26.0),
        });
        let free = Camera::from_spec(ViewSpec::Direction {
            direction: [-1.0, 0.5, 2.0],
            up: None,
        })
        .unwrap();
        // Through the boss's axis, cutting the front half away.
        let plane = Plane {
            point: Vec3::new(0.0, 0.0, 20.0),
            normal: Vec3::Z,
        };
        // A closed boss, as SolidWorks' bodies are, or the cut can't be filled.
        let mut closed = bracket(true);
        closed.faces.truncate(closed.faces.len() - 2);
        closed.faces.extend(cylinder(40.0, 20.0, 6.0, 10.0, 18.0, Some(0)));
        let mut sectioned = request(closed, Camera::section_views(plane.normal).to_vec(), 2, 512);
        sectioned.section = Some(plane);
        let mut zoomed_section = sectioned.clone();
        zoomed_section.model.focus = zoomed.focus;

        let cases = [
            (
                "render_default.png",
                request(bracket(false), standard(&View::DEFAULT_GRID), 2, 512),
                Some(whole_span),
            ),
            (
                "render_highlight.png",
                request(
                    bracket(true),
                    standard(&[View::Isometric, View::IsometricBelow, View::Bottom]),
                    3,
                    384,
                ),
                Some(whole_span),
            ),
            ("render_section.png", sectioned, None),
            ("render_section_zoom.png", zoomed_section, None),
            (
                "render_transparent.png",
                request(transparent, vec![Camera::standard(View::Isometric), free], 2, 512),
                Some(whole_span),
            ),
            (
                "render_zoom.png",
                request(zoomed, standard(&[View::Front, View::Isometric]), 2, 512),
                Some(2.2 * 0.5 * (12f32 * 12. + 8. * 8. + 12. * 12.).sqrt()),
            ),
        ];
        for (name, request, span) in cases {
            let (tile_px, columns) = (request.tile_px, request.columns);
            let rendered = rt.block_on(renderer.render(request)).unwrap();
            assert_eq!(rendered.width, tile_px * columns as u32);
            if let Some(span) = span {
                assert!((rendered.tile_span_mm - span).abs() < 0.01, "{name}: {}", rendered.tile_span_mm);
            }
            std::fs::write(format!("{out_dir}/{name}"), &rendered.png).unwrap();
        }
    }
}

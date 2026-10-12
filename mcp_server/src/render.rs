//! Pictures of the part: the add-in's tessellation drawn headless with
//! kiss3d (wgpu), as a grid of orthographic standard views in one PNG.
//!
//! kiss3d keeps its GPU context and resource managers in thread-locals, and
//! its scene graph is `Rc`-based, so all rendering happens on one dedicated
//! thread that lives as long as the process (spawned on first use). Tool
//! calls hand it a [`Model`] and wait for the PNG on a oneshot channel. The
//! GPU device is created once and reused; if creating it fails (no usable
//! adapter) the error is reported and the next call tries again.

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::{Mutex, mpsc};

use anyhow::{Context as _, Result, anyhow};
use image::{ImageBuffer, Rgb, RgbImage};
use kiss3d::camera::{CoordinateSystem2d, FixedView2d, OrbitCamera3d, Projection};
use kiss3d::color::Color;
use kiss3d::glamx::{Vec2, Vec3};
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

/// A part to draw, in millimeters, model space.
#[derive(Debug, Default, Clone)]
pub struct Model {
    pub faces: Vec<Face>,
    pub edges: Vec<Edge>,
}

#[derive(Debug, Default, Clone)]
pub struct Face {
    /// Three vertices per triangle.
    pub vertices: Vec<[f32; 3]>,
    /// One per vertex (outward), or empty to use flat triangle normals.
    pub normals: Vec<[f32; 3]>,
    /// Index into [`HIGHLIGHTS`].
    pub highlight: Option<usize>,
}

#[derive(Debug, Default, Clone)]
pub struct Edge {
    pub points: Vec<[f32; 3]>,
    pub highlight: Option<usize>,
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
}

impl Target {
    fn face(&self, face: &proto::FaceMesh) -> bool {
        match self {
            Target::Face(id) => face.persist_id == *id,
            Target::Body(id) => face.body_persist_id == *id,
            Target::Feature(id) => face.feature_persist_id == *id,
            Target::FeatureName(name) => face.feature_name == *name,
            Target::Edge(_) => false,
        }
    }

    fn edge(&self, edge: &proto::EdgePolyline) -> bool {
        match self {
            Target::Edge(id) => edge.persist_id == *id,
            Target::Body(id) => edge.body_persist_id == *id,
            _ => false,
        }
    }
}

/// Converts the add-in's mesh (meters) to a [`Model`] (millimeters), with
/// `targets[i]` drawn in `HIGHLIGHTS[i]`; the first matching target wins.
/// Also returns how many faces/edges each target matched.
pub fn model_from_mesh(mesh: &proto::GetMeshResponse, targets: &[Target]) -> (Model, Vec<usize>) {
    let mut matched = vec![0; targets.len()];
    let mut pick = |hit: &dyn Fn(&Target) -> bool| {
        let i = targets.iter().take(HIGHLIGHTS.len()).position(hit)?;
        matched[i] += 1;
        Some(i)
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

    let faces = mesh
        .faces
        .iter()
        .map(|f| Face {
            vertices: points(&f.triangles),
            normals: directions(&f.normals),
            highlight: pick(&|t| t.face(f)),
        })
        .collect();
    let edges = mesh
        .edges
        .iter()
        .map(|e| Edge {
            points: points(&e.points),
            highlight: pick(&|t| t.edge(e)),
        })
        .collect();
    (Model { faces, edges }, matched)
}

impl Model {
    pub fn triangle_count(&self) -> usize {
        self.faces.iter().map(|f| f.vertices.len() / 3).sum()
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

    /// Unit (right, up, toward-eye) screen axes in model space.
    fn basis(self) -> (Vec3, Vec3, Vec3) {
        let (toward_eye, up_hint) = self.orientation();
        let right = up_hint.cross(toward_eye).normalize();
        let up = toward_eye.cross(right);
        (right, up, toward_eye)
    }

    /// Which model directions point right and up on screen, e.g.
    /// "+X right, -Z up", for orthographic views; None for the isometric ones.
    pub fn screen_axes(self) -> Option<String> {
        let (right, up, _) = self.basis();
        Some(format!(
            "{} right, {} up",
            axis_name(right)?,
            axis_name(up)?
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
    pub views: Vec<View>,
    pub columns: usize,
    /// Side of each (square) tile, pixels.
    pub tile_px: u32,
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

/// Center and radius of the model's bounding sphere (from its box).
fn bounds(model: &Model) -> Option<(Vec3, f32)> {
    let points = model
        .faces
        .iter()
        .flat_map(|f| f.vertices.iter())
        .chain(model.edges.iter().flat_map(|e| e.points.iter()))
        .map(|p| Vec3::from_array(*p));
    let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
    for p in points {
        lo = lo.min(p);
        hi = hi.max(p);
    }
    if !(lo.is_finite() && hi.is_finite()) {
        return None;
    }
    let radius = (hi - lo).length() / 2.0;
    Some(((lo + hi) / 2.0, if radius > 0.0 { radius } else { 1.0 }))
}

fn render_job(window: &mut Option<Window>, request: &Request) -> Result<Rendered> {
    let (center, radius) =
        bounds(&request.model).ok_or_else(|| anyhow!("the part has no geometry to draw"))?;
    // Everything is drawn in a normalized space: centered, bounding radius
    // 1. Keeps camera distances and clip planes independent of part size.
    let to_scene = |p: &[f32; 3]| (Vec3::from_array(*p) - center) / radius;

    let tile = request.tile_px;
    if window
        .as_ref()
        .is_none_or(|w| w.size().x != tile || w.size().y != tile)
    {
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
    // One mesh per color: the plain part, then each highlight.
    for group in std::iter::once(None).chain((0..HIGHLIGHTS.len()).map(Some)) {
        let faces: Vec<&Face> = request
            .model
            .faces
            .iter()
            .filter(|f| f.highlight == group)
            .collect();
        if faces.is_empty() {
            continue;
        }
        let mut coords = Vec::new();
        let mut normals = Vec::new();
        for face in &faces {
            let n = face.vertices.len() / 3 * 3;
            coords.extend(face.vertices[..n].iter().map(to_scene));
            if face.normals.len() == face.vertices.len() {
                normals.extend(
                    face.normals[..n]
                        .iter()
                        .map(|v| Vec3::from_array(*v).normalize_or_zero()),
                );
            } else {
                for t in face.vertices[..n].chunks_exact(3) {
                    let [a, b, c] = [0, 1, 2].map(|i| Vec3::from_array(t[i]));
                    let normal = (b - a).cross(c - a).normalize_or_zero();
                    normals.extend([normal; 3]);
                }
            }
        }
        let indices = (0..coords.len() as u32 / 3)
            .map(|i| [3 * i, 3 * i + 1, 3 * i + 2])
            .collect();
        let mesh = GpuMesh3d::new(coords, indices, Some(normals), None, false);
        let rgb = group.map_or(PART_COLOR, |i| HIGHLIGHTS[i].1);
        scene
            .add_mesh(Rc::new(RefCell::new(mesh)), Vec3::ONE)
            .set_color(color(rgb))
            .set_metallic(0.0)
            .set_roughness(0.8)
            // SolidWorks' winding isn't guaranteed; draw both sides.
            .enable_backface_culling(false);
    }

    let edges: Vec<Polyline3d> = request
        .model
        .edges
        .iter()
        .filter(|e| e.points.len() >= 2)
        .map(|e| {
            let line = Polyline3d::new(e.points.iter().map(to_scene).collect());
            match e.highlight {
                // Pulled slightly toward the viewer so they win over the faces
                // they bound (kiss3d: positive bias = nearer, as a fraction of
                // depth). Highlighted edges are thicker and pulled further.
                Some(i) => line
                    .with_color(color(HIGHLIGHTS[i].1))
                    .with_width(5.0)
                    .with_depth_bias(0.006),
                None => line
                    .with_color(color(EDGE_COLOR))
                    .with_width(1.5)
                    .with_depth_bias(0.003),
            }
        })
        .collect();

    let font = Font::default();
    let columns = request.columns.max(1);
    let rows = request.views.len().div_ceil(columns);
    let (width, height) = (tile * columns as u32, tile * rows as u32);
    let mut out: RgbImage = ImageBuffer::from_pixel(width, height, Rgb([255, 255, 255]));

    // Orthographic: the frame is set by distance * tan(fov / 2). Frame the
    // bounding sphere (radius 1) with a 10% margin.
    const DIST: f32 = 4.0;
    const HALF_SPAN: f32 = 1.1;
    let fov = 2.0 * (HALF_SPAN / DIST).atan();

    for (i, view) in request.views.iter().enumerate() {
        let (right, up, toward_eye) = view.basis();
        let mut camera =
            OrbitCamera3d::new_with_frustum(fov, 0.1, 2.0 * DIST, toward_eye * DIST, Vec3::ZERO);
        camera.set_up_axis_dir(up);
        camera.look_at(toward_eye * DIST, Vec3::ZERO);
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
            view.label(),
            Vec2::new(px * 0.03, px * 0.02),
            px * 0.06,
            &font,
            color([0.2, 0.2, 0.25]),
        );
        draw_triad(window, &font, *view, px);

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
    let span_mm = 2.0 * HALF_SPAN * radius;
    Ok(Rendered {
        png,
        width,
        height,
        tile_span_mm: span_mm,
    })
}

/// Model axes in the bottom-left corner: X red, Y green, Z blue, as in
/// SolidWorks. Axes pointing (nearly) at the viewer are drawn as a dot.
fn draw_triad(window: &mut Window, font: &std::sync::Arc<Font>, view: View, px: f32) {
    let (right, up, _) = view.basis();
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
    fn mesh_converts_to_mm_and_highlights_first_match() {
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
            ],
            edges: vec![proto::EdgePolyline {
                persist_id: b"e1".to_vec(),
                body_persist_id: b"b1".to_vec(),
                points: vec![0.0, 0.0, 0.0, 0.001, 0.0, 0.0],
            }],
            ..Default::default()
        };
        let targets = [
            Target::Face(b"f2".to_vec()),
            Target::FeatureName("Cut-Extrude1".into()),
            Target::Body(b"b1".to_vec()),
            Target::Edge(b"nope".to_vec()),
        ];
        let (model, matched) = model_from_mesh(&mesh, &targets);
        assert_eq!(model.faces[0].vertices[1], [10.0, 0.0, 0.0]);
        assert_eq!(model.faces[0].normals[0], [0.0, 0.0, 1.0]);
        assert_eq!(model.edges[0].points[1], [1.0, 0.0, 0.0]);
        // f1 only matches the body; f2 matches the face first.
        assert_eq!(model.faces[0].highlight, Some(2));
        assert_eq!(model.faces[1].highlight, Some(0));
        assert_eq!(model.edges[0].highlight, Some(2));
        assert_eq!(matched, vec![1, 0, 2, 0]);
        assert_eq!(model.triangle_count(), 2);
    }

    #[test]
    fn screen_axes_match_solidworks_views() {
        assert_eq!(View::Front.screen_axes().unwrap(), "+X right, +Y up");
        assert_eq!(View::Top.screen_axes().unwrap(), "+X right, -Z up");
        assert_eq!(View::Right.screen_axes().unwrap(), "-Z right, +Y up");
        assert_eq!(View::Back.screen_axes().unwrap(), "-X right, +Y up");
        assert_eq!(View::Left.screen_axes().unwrap(), "+Z right, +Y up");
        assert_eq!(View::Bottom.screen_axes().unwrap(), "+X right, +Z up");
        assert!(View::Isometric.screen_axes().is_none());
    }

    /// A quad a-b-c-d (counter-clockwise seen from outside) and its outline.
    fn quad(model: &mut Model, q: [[f32; 3]; 4], highlight: Option<usize>) {
        let [a, b, c, d] = q;
        model.faces.push(Face {
            vertices: vec![a, b, c, a, c, d],
            normals: vec![],
            highlight,
        });
        for (p, q) in [(a, b), (b, c), (c, d), (d, a)] {
            model.edges.push(Edge {
                points: vec![p, q],
                highlight: None,
            });
        }
    }

    /// An L-shaped bracket (60 x 40 x 30 mm) with a round boss on its foot.
    fn bracket(highlight_boss: bool) -> Model {
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
                    normals: vec![],
                    highlight: None,
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
        let cases = [
            (
                "render_default.png",
                View::DEFAULT_GRID.to_vec(),
                2,
                512,
                false,
            ),
            (
                "render_highlight.png",
                vec![View::Isometric, View::IsometricBelow, View::Bottom],
                3,
                384,
                true,
            ),
        ];
        for (name, views, columns, tile_px, highlight) in cases {
            let request = Request {
                model: bracket(highlight),
                views: views.clone(),
                columns,
                tile_px,
            };
            let rendered = rt.block_on(renderer.render(request)).unwrap();
            assert_eq!(rendered.width, tile_px * columns as u32);
            assert!(
                (rendered.tile_span_mm - 2.2 * 0.5 * (60f32 * 60. + 30. * 30. + 40. * 40.).sqrt())
                    .abs()
                    < 0.01
            );
            std::fs::write(format!("{out_dir}/{name}"), &rendered.png).unwrap();
        }
    }
}

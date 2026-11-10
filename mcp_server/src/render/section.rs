//! Section views on the CPU: the model cut by a plane before drawing, the
//! cut's outline and its cut faces (caps). No GPU here: [`super`] draws the
//! result. architecture.md "GetMesh / rendering".

use std::collections::{HashMap, VecDeque};

use kiss3d::glamx::Vec2;

use super::{Edge, Face, Model, Vec3};

/// A section plane: everything on the side `normal` points to is cut away.
#[derive(Debug, Clone, Copy)]
pub struct Plane {
    /// A point on the plane, mm.
    pub point: Vec3,
    /// Unit length.
    pub normal: Vec3,
}

impl Plane {
    /// Signed distance, positive on the cut-away side.
    fn side(&self, p: Vec3) -> f32 {
        (p - self.point).dot(self.normal)
    }

    /// Whether clipping keeps `p`: points on the plane stay, so the cut lies
    /// exactly in it.
    fn keeps(&self, p: Vec3) -> bool {
        self.side(p) <= 0.0
    }

    /// Where the segment a-b crosses the plane (0..=1 from a), if [`Self::keeps`]
    /// one end and not the other.
    fn crossing(&self, a: Vec3, b: Vec3) -> Option<f32> {
        let (da, db) = (self.side(a), self.side(b));
        (self.keeps(a) != self.keeps(b)).then(|| da / (da - db))
    }

    /// Whether the cut leaves any material: some face vertex strictly on the
    /// kept side. Stricter than [`Self::keeps`], so a plane that only touches
    /// the model (a block cut at one of its own faces) keeps nothing.
    /// render_views refuses such a plane before drawing.
    pub fn keeps_any(&self, model: &Model) -> bool {
        model
            .faces
            .iter()
            .flat_map(|f| &f.vertices)
            .any(|p| self.side(Vec3::from_array(*p)) < 0.0)
    }
}

/// A model cut by a [`Plane`].
#[derive(Debug, Default)]
pub struct Section {
    /// What's left.
    pub model: Model,
    /// Where faces cross the plane, as polylines.
    pub outline: Vec<Vec<[f32; 3]>>,
    /// The cut faces: the plane where it's inside (opaque) material, one
    /// per highlight group, normals = the plane's.
    pub caps: Vec<Face>,
}

/// `model` with everything on the normal side of `plane` cut away, its
/// outline and the cut faces.
pub fn section(model: &Model, plane: &Plane) -> Section {
    let mut faces = Vec::new();
    let mut segments = Vec::new();
    // Per highlight group: the cut segments of opaque faces, for the caps.
    let mut boundaries: Vec<(Option<usize>, Vec<[Vec3; 2]>)> = Vec::new();
    for face in &model.faces {
        let (kept, cuts) = clip_face(face, plane);
        if !face.transparent && !cuts.is_empty() {
            match boundaries.iter_mut().find(|(h, _)| *h == face.highlight) {
                Some((_, list)) => list.extend(&cuts),
                None => boundaries.push((face.highlight, cuts.clone())),
            }
        }
        segments.extend(cuts);
        if !kept.vertices.is_empty() {
            faces.push(kept);
        }
    }
    let edges = model.edges.iter().flat_map(|e| clip_edge(e, plane)).collect();
    let caps = boundaries
        .into_iter()
        .map(|(highlight, boundary)| {
            let vertices = fill(&boundary, plane);
            Face {
                normals: vec![plane.normal.to_array(); vertices.len()],
                vertices,
                highlight,
                transparent: false,
            }
        })
        .filter(|f| !f.vertices.is_empty())
        .collect();
    let size = model.bounding_box().map_or(1.0, |b| (b.hi - b.lo).length());
    Section {
        model: Model {
            faces,
            edges,
            focus: model.focus,
        },
        outline: chain(&segments, size * 1e-5),
        caps,
    }
}

/// A triangle corner: position and normal (zero when the face has none).
type Corner = (Vec3, Vec3);

/// The part of `face` that `plane` keeps (winding kept, normals
/// interpolated), and one segment per triangle crossing the plane, turned
/// as [`oriented`] says.
fn clip_face(face: &Face, plane: &Plane) -> (Face, Vec<[Vec3; 2]>) {
    let smooth = face.normals.len() == face.vertices.len();
    let mut kept = Face {
        highlight: face.highlight,
        transparent: face.transparent,
        ..Default::default()
    };
    let mut cuts = Vec::new();
    for (i, tri) in face.vertices.chunks_exact(3).enumerate() {
        let corners: [Corner; 3] = std::array::from_fn(|k| {
            let normal = if smooth { Vec3::from_array(face.normals[3 * i + k]) } else { Vec3::ZERO };
            (Vec3::from_array(tri[k]), normal)
        });
        let (polygon, cut) = clip_triangle(&corners, plane);
        if let [a, b] = cut[..] {
            cuts.push(oriented(a, b, &corners, smooth, plane));
        }
        // A fan: the polygon is convex.
        for k in 1..polygon.len().saturating_sub(1) {
            for (p, n) in [polygon[0], polygon[k], polygon[k + 1]] {
                kept.vertices.push(p.to_array());
                if smooth {
                    kept.normals.push(n.to_array());
                }
            }
        }
    }
    (kept, cuts)
}

/// Sutherland-Hodgman against one plane: the kept polygon (no corners, a
/// triangle or a quad, in the triangle's order) and where its sides cross
/// the plane.
fn clip_triangle(corners: &[Corner; 3], plane: &Plane) -> (Vec<Corner>, Vec<Vec3>) {
    let mut polygon = Vec::with_capacity(4);
    let mut cut = Vec::with_capacity(2);
    for k in 0..3 {
        let ((a, na), (b, nb)) = (corners[k], corners[(k + 1) % 3]);
        if plane.keeps(a) {
            polygon.push((a, na));
        }
        if let Some(t) = plane.crossing(a, b) {
            let p = a.lerp(b, t);
            polygon.push((p, na.lerp(nb, t).normalize_or_zero()));
            cut.push(p);
        }
    }
    (polygon, cut)
}

/// The cut segment a-b turned so the triangle's material is on its left,
/// seen from the normal side. The triangle's outward normal (its vertex
/// normals, else its winding), projected into the plane, points away from
/// the material, so the segment runs along `normal × outward`.
fn oriented(a: Vec3, b: Vec3, corners: &[Corner; 3], smooth: bool, plane: &Plane) -> [Vec3; 2] {
    let outward = if smooth {
        corners.iter().map(|c| c.1).sum()
    } else {
        let [p, q, r] = corners.map(|c| c.0);
        (q - p).cross(r - p)
    };
    if (b - a).dot(plane.normal.cross(outward)) >= 0.0 { [a, b] } else { [b, a] }
}

/// The runs of `edge` that `plane` keeps.
fn clip_edge(edge: &Edge, plane: &Plane) -> Vec<Edge> {
    let mut runs = Vec::new();
    let mut run: Vec<[f32; 3]> = Vec::new();
    for pair in edge.points.windows(2) {
        let (a, b) = (Vec3::from_array(pair[0]), Vec3::from_array(pair[1]));
        if plane.keeps(a) && run.is_empty() {
            run.push(a.to_array());
        }
        if let Some(t) = plane.crossing(a, b) {
            run.push(a.lerp(b, t).to_array());
        }
        if plane.keeps(b) {
            run.push(b.to_array());
        } else if !run.is_empty() {
            runs.push(std::mem::take(&mut run));
        }
    }
    runs.push(run);
    runs.into_iter()
        .filter(|points| points.len() >= 2)
        .map(|points| Edge { points, ..edge.clone() })
        .collect()
}

/// Triangles covering, in `plane`, what `boundary` encloses (nonzero
/// winding; each segment has the inside on its left seen from the normal
/// side). A sweep: between each two consecutive end-point heights, the
/// segments crossing that slab are sorted left to right and the stretches
/// inside become trapezoids. Needs no closed loops, so the small gaps
/// between faces' tessellations only leave slivers out.
fn fill(boundary: &[[Vec3; 2]], plane: &Plane) -> Vec<[f32; 3]> {
    let u = plane.normal.any_orthonormal_vector();
    // (u, v, normal) right-handed: counter-clockwise seen from the normal side.
    let v = plane.normal.cross(u);
    let flat = |p: Vec3| {
        let d = p - plane.point;
        Vec2::new(d.dot(u), d.dot(v))
    };
    let lift = |q: Vec2| (plane.point + u * q.x + v * q.y).to_array();
    // (lower end, upper end, +1 if it runs downward: entering the inside
    // when crossed left to right; -1 upward).
    let mut segments: Vec<(Vec2, Vec2, i32)> = boundary
        .iter()
        .filter_map(|&[a, b]| {
            let (a, b) = (flat(a), flat(b));
            match a.y.total_cmp(&b.y) {
                std::cmp::Ordering::Less => Some((a, b, -1)),
                std::cmp::Ordering::Greater => Some((b, a, 1)),
                std::cmp::Ordering::Equal => None,
            }
        })
        .collect();
    segments.sort_by(|a, b| a.0.y.total_cmp(&b.0.y));
    let mut heights: Vec<f32> = segments.iter().flat_map(|s| [s.0.y, s.1.y]).collect();
    heights.sort_by(f32::total_cmp);
    heights.dedup();
    let x_at = |s: &(Vec2, Vec2, i32), y: f32| s.0.x + (s.1.x - s.0.x) * (y - s.0.y) / (s.1.y - s.0.y);

    let mut triangles = Vec::new();
    let mut active: Vec<usize> = Vec::new();
    let mut next = 0;
    for slab in heights.windows(2) {
        let (y0, y1) = (slab[0], slab[1]);
        while next < segments.len() && segments[next].0.y <= y0 {
            active.push(next);
            next += 1;
        }
        active.retain(|&i| segments[i].1.y > y0);
        let middle = (y0 + y1) / 2.0;
        let mut crossing: Vec<(f32, usize)> = active.iter().map(|&i| (x_at(&segments[i], middle), i)).collect();
        crossing.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut winding = 0;
        for pair in crossing.windows(2) {
            let (left, right) = (&segments[pair[0].1], &segments[pair[1].1]);
            winding += left.2;
            if winding != 0 {
                let quad = [
                    Vec2::new(x_at(left, y0), y0),
                    Vec2::new(x_at(right, y0), y0),
                    Vec2::new(x_at(right, y1), y1),
                    Vec2::new(x_at(left, y1), y1),
                ];
                triangles.extend([0, 1, 2, 0, 2, 3].map(|k| lift(quad[k])));
            }
        }
    }
    triangles
}

/// Joins segments that share end points (within `tolerance`) into
/// polylines, so the outline isn't thousands of separate lines.
fn chain(segments: &[[Vec3; 2]], tolerance: f32) -> Vec<Vec<[f32; 3]>> {
    let tolerance = tolerance.max(f32::MIN_POSITIVE);
    let key = |p: Vec3| (p / tolerance).round().as_ivec3().to_array();
    let mut at: HashMap<[i32; 3], Vec<usize>> = HashMap::new();
    for (i, s) in segments.iter().enumerate() {
        for p in s {
            at.entry(key(*p)).or_default().push(i);
        }
    }
    let mut used = vec![false; segments.len()];
    // The unused segment touching `p`, and its other end.
    let next = |p: Vec3, used: &mut Vec<bool>| {
        let i = *at.get(&key(p))?.iter().find(|&&i| !used[i])?;
        used[i] = true;
        let [a, b] = segments[i];
        Some(if key(a) == key(p) { b } else { a })
    };
    let mut lines = Vec::new();
    for i in 0..segments.len() {
        if used[i] {
            continue;
        }
        used[i] = true;
        let [a, b] = segments[i];
        let mut line = VecDeque::from([a, b]);
        while let Some(p) = next(*line.back().unwrap(), &mut used) {
            line.push_back(p);
        }
        while let Some(p) = next(*line.front().unwrap(), &mut used) {
            line.push_front(p);
        }
        lines.push(line.into_iter().map(|p| p.to_array()).collect());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::super::tests::{bracket, cylinder};
    use super::*;

    #[test]
    fn section_clips_faces_and_edges_and_outlines_the_cut() {
        let model = bracket(false);
        // Through the middle of the bracket's depth (z 0..40), cutting z > 20 away.
        let plane = Plane {
            point: Vec3::new(0.0, 0.0, 20.0),
            normal: Vec3::Z,
        };
        let Section { model: cut, outline, .. } = section(&model, &plane);
        let points = cut.faces.iter().flat_map(|f| &f.vertices).chain(cut.edges.iter().flat_map(|e| &e.points));
        for p in points {
            assert!(p[2] <= 20.0 + 1e-4, "{p:?} is on the cut-away side");
        }
        let b = cut.bounding_box().unwrap();
        assert!((b.hi.z - 20.0).abs() < 1e-4 && b.lo.z == 0.0);
        // The front end (z = 40, two faces) is gone; the rest is cut or whole.
        assert_eq!(cut.faces.len(), model.faces.len() - 2);
        for f in &cut.faces {
            assert_eq!(f.vertices.len() % 3, 0);
            assert!(f.normals.is_empty() || f.normals.len() == f.vertices.len());
        }
        // The boss (y 10..18 at z 14..26) and the L: each cut once, a closed loop
        // each (the boss's side face is open where it meets the foot, so it's
        // two arcs that meet only at their ends).
        for line in &outline {
            assert!(line.len() >= 2);
        }
        let closed = |line: &Vec<[f32; 3]>| {
            Vec3::from_array(line[0]).distance(Vec3::from_array(*line.last().unwrap())) < 1e-3
        };
        assert!(outline.iter().any(|l| closed(l) && l.len() >= 7), "the L isn't one closed loop: {outline:?}");

        // Cutting nothing away keeps everything.
        let beyond = Plane {
            point: Vec3::new(0.0, 0.0, 50.0),
            normal: Vec3::Z,
        };
        let all = section(&model, &beyond);
        assert_eq!(all.model.triangle_count(), model.triangle_count());
        assert_eq!(all.model.edges.len(), model.edges.len());
        assert!(all.outline.is_empty() && all.caps.is_empty());
    }

    #[test]
    fn keeps_any_needs_material_strictly_on_the_kept_side() {
        let model = bracket(false); // z 0..40
        let at = |z: f32| Plane { point: Vec3::new(0.0, 0.0, z), normal: Vec3::Z };
        assert!(at(20.0).keeps_any(&model));
        // Cut at its back face: only that face's plane would be left.
        assert!(!at(0.0).keeps_any(&model));
        assert!(!at(-5.0).keeps_any(&model));
    }

    fn area(face: &Face) -> f32 {
        face.vertices
            .chunks_exact(3)
            .map(|t| {
                let [a, b, c] = [0, 1, 2].map(|k| Vec3::from_array(t[k]));
                (b - a).cross(c - a).length() / 2.0
            })
            .sum()
    }

    #[test]
    fn section_caps_cover_the_cut_material() {
        // Closed solids: the bracket's L prism, and a cylinder beside it
        // (radius 6, 8 high) that's highlighted.
        let mut model = bracket(false);
        model.faces.truncate(model.faces.len() - 2); // the bracket's open boss
        model.faces.extend(cylinder(100.0, 20.0, 6.0, 0.0, 8.0, Some(0)));
        let cut = |normal: Vec3, point: Vec3| {
            section(&model, &Plane { point, normal })
                .caps
                .iter()
                .map(|c| (c.highlight, area(c).round()))
                .collect::<Vec<_>>()
        };
        // Through the middle of the depth: the L (60 x 10 foot + 10 x 20 wall
        // on it), and the cylinder through its axis (2 r x 8, a little less:
        // it's a 48-gon with no corner on the plane), the other way round too.
        let z = Vec3::new(0.0, 0.0, 20.0);
        let expected = [(None, 800.0), (Some(0), 96.0)];
        assert_eq!(cut(Vec3::Z, z), expected);
        assert_eq!(cut(Vec3::NEG_Z, z), expected);
        // Across the foot: 10 x 40.
        assert_eq!(cut(Vec3::X, Vec3::new(30.0, 0.0, 0.0)), [(None, 400.0)]);
        // Across the wall and the cylinder, tilted: still whole areas.
        let tilted = section(&model, &Plane { point: Vec3::new(0.0, 4.0, 20.0), normal: Vec3::new(0.0, 1.0, 0.3).normalize() });
        assert_eq!(tilted.caps.len(), 2);
        let cap = &section(&model, &Plane { point: z, normal: Vec3::Z }).caps[0];
        assert!(cap.vertices.iter().all(|p| (p[2] - 20.0).abs() < 1e-4));
        assert!(cap.normals.iter().all(|n| *n == [0.0, 0.0, 1.0]));
        // Transparent faces get no cap.
        let mut glass = model.clone();
        glass.faces.iter_mut().for_each(|f| f.transparent = true);
        assert!(section(&glass, &Plane { point: z, normal: Vec3::Z }).caps.is_empty());
    }

    #[test]
    fn chain_joins_segments_in_any_order_and_direction() {
        let p = |x: f32, y: f32| Vec3::new(x, y, 0.0);
        // A square, shuffled, one segment reversed; plus a separate segment.
        let segments = [
            [p(1.0, 1.0), p(0.0, 1.0)],
            [p(0.0, 0.0), p(1.0, 0.0)],
            [p(5.0, 5.0), p(6.0, 5.0)],
            [p(1.0, 1.0), p(1.0, 0.0)],
            [p(0.0, 1.0), p(0.0, 0.0)],
        ];
        let mut lines = chain(&segments, 1e-4);
        lines.sort_by_key(|l| l.len());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 2);
        assert_eq!(lines[1].len(), 5);
        assert_eq!(lines[1][0], lines[1][4]);
    }
}

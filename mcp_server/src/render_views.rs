//! The render_views tool: its params, the cameras and section plane they
//! ask for, the highlight / hide / transparent / zoom lists resolved to
//! render targets, and the mesh (cached between renders).

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::IdRegistry;
use crate::{SolidWorksTools, render};

pub const DEFAULT_TILE_PX: u32 = 512;
pub const TILE_PX_RANGE: std::ops::RangeInclusive<u32> = 256..=1024;
pub const MAX_VIEWS: usize = 9;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RenderViewsParams {
    /// Views to draw, in order, as a grid: a standard view's name, or {"direction": [x, y, z]}
    /// to look from any direction (model axes, from the part toward the eye; optional "up").
    /// Defaults to ["top", "isometric", "front", "right"] in a 2x2 grid: top above front and
    /// right beside it, like a third-angle drawing; with `section`, straight at the cut and at
    /// an angle.
    #[serde(default)]
    pub views: Option<Vec<render::ViewSpec>>,
    /// Entities to color in, each an id from inspect_model ("face:7", "edge:3", "body:1",
    /// "feature:5", or in an assembly "component:2") or a feature name ("Cut-Extrude1", for the
    /// faces it made). Up to 6; each gets its own color, reported in the result.
    #[serde(default)]
    pub highlight: Vec<String>,
    /// Entities to leave out of this picture (ids or feature names, as for highlight), e.g. a
    /// housing to see inside an assembly. Nothing is hidden in SolidWorks.
    #[serde(default)]
    pub hide: Vec<String>,
    /// Entities to draw see-through (ids or feature names), or ["others"] for everything not
    /// highlighted.
    #[serde(default)]
    pub transparent: Vec<String>,
    /// Frame these (ids or feature names) instead of the whole model, to zoom in on them.
    #[serde(default)]
    pub zoom_to: Vec<String>,
    /// Cut the model with a plane; the cut material is drawn solid (tan, or a darker
    /// highlight color).
    #[serde(default)]
    pub section: Option<SectionParams>,
    /// Size of each view in pixels (square), 256 to 1024. Defaults to 512.
    #[serde(default)]
    pub size_px: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SectionParams {
    /// The plane's normal, model axes; everything on the side it points to is cut away.
    pub normal: [f32; 3],
    /// A point on the plane, model mm. Defaults to the middle of what's shown.
    #[serde(default)]
    pub point_mm: Option<[f32; 3]>,
}

/// render_views' id lists, trimmed, as given: what its result names.
pub struct PickNames {
    pub highlight: Vec<String>,
    pub hide: Vec<String>,
    pub transparent: Vec<String>,
    pub zoom_to: Vec<String>,
    /// `transparent: ["others"]`.
    pub others: bool,
}

/// render_views' tiles: `views`, else the default grid, or with a section
/// the two section views.
pub fn render_cameras(
    views: Option<&[render::ViewSpec]>,
    section_normal: Option<render::Vec3>,
) -> Result<Vec<render::Camera>, McpError> {
    match (views, section_normal) {
        (None, None) => Ok(render::View::DEFAULT_GRID
            .map(render::Camera::standard)
            .to_vec()),
        (None, Some(normal)) => Ok(render::Camera::section_views(normal).to_vec()),
        (Some(views), _) if views.is_empty() || views.len() > MAX_VIEWS => Err(
            McpError::invalid_params(format!("give between 1 and {MAX_VIEWS} views"), None),
        ),
        (Some(views), _) => views
            .iter()
            .map(|&view| render::Camera::from_spec(view))
            .collect::<Result<_, _>>()
            .map_err(|err| McpError::invalid_params(err, None)),
    }
}

/// As square as possible: 1-3 in a row, 4 as 2x2, 5-6 as 3x2, ...
pub fn grid_columns(views: usize) -> usize {
    match views {
        n @ 1..=3 => n,
        4 => 2,
        _ => 3,
    }
}

/// "through (x, y, z) mm, normal (x, y, z)", for render_views' messages.
pub fn describe_plane(plane: &render::Plane) -> String {
    let (p, n) = (plane.point, plane.normal);
    format!(
        "through ({:.1}, {:.1}, {:.1}) mm, normal ({:.2}, {:.2}, {:.2})",
        p.x, p.y, p.z, n.x, n.y, n.z
    )
}

/// The section plane through `section.point_mm`, or the middle of `model`;
/// refused if it cuts away all of `model`.
pub fn section_plane(
    section: &SectionParams,
    normal: render::Vec3,
    model: &render::Model,
) -> Result<render::Plane, McpError> {
    let point = section.point_mm.map_or_else(
        || {
            model
                .bounding_box()
                .map_or(render::Vec3::ZERO, |b| b.center())
        },
        render::Vec3::from_array,
    );
    let plane = render::Plane { point, normal };
    if !plane.keeps_any(model) {
        return Err(McpError::invalid_params(
            format!(
                "the section plane {} cuts away everything that's shown: the side its normal \
                 points to is removed",
                describe_plane(&plane)
            ),
            None,
        ));
    }
    Ok(plane)
}

/// render_views' result lines for what `names` asked for: each one's color
/// or what was done with it, or that it isn't visible.
pub fn describe_picks(names: &PickNames, matched: &render::Matched) -> String {
    let mut text = String::new();
    if !names.zoom_to.is_empty() && matched.zoom.iter().any(|&n| n > 0) {
        text += "\nZoomed in: tiles frame what zoom_to names.";
    }
    if names.others {
        text += "\nEverything not highlighted is transparent.";
    }
    let not_visible = "not visible (deleted, hidden, or on a hidden body?)";
    for ((name, count), (color, _)) in names
        .highlight
        .iter()
        .zip(&matched.highlight)
        .zip(render::HIGHLIGHTS)
    {
        text += &match count {
            0 => format!("\n{name}: {not_visible}"),
            _ => format!("\n{name}: {color}"),
        };
    }
    for (list, counts, done) in [
        (&names.hide, &matched.hide, "left out"),
        (&names.transparent, &matched.transparent, "transparent"),
        (&names.zoom_to, &matched.zoom, "framed"),
    ] {
        for (name, count) in list.iter().zip(counts) {
            text += &match count {
                0 => format!("\n{name}: {not_visible}"),
                _ => format!("\n{name}: {done}"),
            };
        }
    }
    text
}

impl SolidWorksTools {
    /// The active document's mesh, from the add-in or, when the add-in says
    /// it hasn't changed, from the last render (unplaced either way).
    pub(crate) async fn mesh(&self) -> Result<proto::GetMeshResponse, McpError> {
        let lock = || {
            self.mesh
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        };
        let mut cached_key = lock().as_ref().map_or(0, |m| m.key);
        loop {
            let reply = self
                .addin
                .get_mesh(proto::GetMeshRequest {
                    include_edges: true,
                    cached_key,
                })
                .await
                .map_err(|status| self.addin_error(status))?;
            let mut cache = lock();
            if !reply.unchanged {
                *cache = Some(reply.clone());
                return Ok(reply);
            }
            match cache.as_ref() {
                Some(mesh) if mesh.key == reply.key => {
                    tracing::debug!("render_views: geometry unchanged, using the cached mesh");
                    return Ok(mesh.clone());
                }
                // Another render replaced the cache meanwhile: ask for all of it.
                _ if cached_key != 0 => cached_key = 0,
                _ => {
                    return Err(McpError::internal_error(
                        "the add-in sent an empty mesh",
                        None,
                    ));
                }
            }
        }
    }

    /// render_views' highlight / hide / transparent / zoom_to lists,
    /// resolved, and their names for the result.
    pub(crate) fn render_picks(
        &self,
        params: &RenderViewsParams,
    ) -> Result<(render::Picks, PickNames), McpError> {
        if params.highlight.len() > render::HIGHLIGHTS.len() {
            return Err(McpError::invalid_params(
                format!("at most {} highlights", render::HIGHLIGHTS.len()),
                None,
            ));
        }
        let trimmed = |list: &[String]| {
            list.iter()
                .map(|h| h.trim().to_string())
                .collect::<Vec<_>>()
        };
        let (others, transparent): (Vec<String>, Vec<String>) = trimmed(&params.transparent)
            .into_iter()
            .partition(|t| t.eq_ignore_ascii_case("others"));
        let names = PickNames {
            highlight: trimmed(&params.highlight),
            hide: trimmed(&params.hide),
            transparent,
            zoom_to: trimmed(&params.zoom_to),
            others: !others.is_empty(),
        };
        let targets = |list: &[String]| {
            list.iter()
                .map(|h| self.highlight_target(h))
                .collect::<Result<Vec<_>, _>>()
        };
        let picks = render::Picks {
            highlight: targets(&names.highlight)?,
            hide: targets(&names.hide)?,
            transparent: targets(&names.transparent)?,
            transparent_rest: names.others,
            zoom: targets(&names.zoom_to)?,
        };
        Ok((picks, names))
    }

    /// What a render_views highlight refers to: a short id, or else a
    /// feature name.
    pub(crate) fn highlight_target(&self, highlight: &str) -> Result<render::Target, McpError> {
        if !IdRegistry::looks_like_id(highlight) {
            if highlight.is_empty() {
                return Err(McpError::invalid_params("empty highlight", None));
            }
            return Ok(render::Target::FeatureName(highlight.to_string()));
        }
        let persist_id = self.resolve_id(highlight)?;
        let kind = highlight.split(':').next().unwrap_or_default();
        Ok(match kind {
            "face" => render::Target::Face(persist_id),
            "edge" => render::Target::Edge(persist_id),
            "body" => render::Target::Body(persist_id),
            "component" => render::Target::Component(persist_id),
            "vertex" => {
                return Err(McpError::invalid_params(
                    format!(
                        "{highlight} is a vertex; render_views can't highlight those -- highlight an edge at it"
                    ),
                    None,
                ));
            }
            _ => render::Target::Feature(persist_id),
        })
    }
}

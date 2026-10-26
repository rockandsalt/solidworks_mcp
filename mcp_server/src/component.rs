//! Inserting components into an assembly: the insert_components tool's
//! params, turning each placement (origin + optional axes) into the
//! orthonormal frame the add-in hands to AddComponents3, and the reply.
//! Also replace_components: swapping components for another file, and the
//! report of which mates came through; fix_components: fixing or floating
//! components; and move_component: moving one while SolidWorks solves its
//! mates, with how far it got (and, checked for collisions, what it hit on
//! the way: the report is motion.rs's).
//!
//! Parts and subassemblies go in the same way (a subassembly is just an
//! .SLDASM file); several instances of one file are several placements, and
//! `copy_of` takes the file and configuration from a component already in
//! the assembly.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use glam::{DMat3, DQuat, DVec3};

use crate::addin::proto;
use crate::assembly::{self, placement_text};
use crate::{delete, edit, files, motion};
use crate::inspect::{IdRegistry, Kind, MM_PER_M, direction as unit_text, mm, point_mm, round};

/// More than this in one call is almost certainly a mistake (and slow).
const MAX_INSTANCES: usize = 100;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InsertComponentsParams {
    /// The file to insert, as an absolute path: a part (.SLDPRT) or an assembly (.SLDASM,
    /// which becomes a subassembly). Give this or copy_of.
    #[serde(default)]
    pub file: Option<String>,
    /// Another instance of a component already in the assembly, by id ("component:3" from
    /// inspect_model): same file and configuration. Give this or file.
    #[serde(default)]
    pub copy_of: Option<String>,
    /// The configuration to use. Default: the file's active one (with copy_of: that
    /// component's).
    #[serde(default)]
    pub configuration: Option<String>,
    /// Where the new components go, one component per entry: list several to insert the
    /// same part several times. Default: one, at the assembly origin, square to the assembly.
    #[serde(default)]
    pub at: Option<Vec<PlacementParams>>,
    /// true: fix the new components in place; false: leave them floating, free to be moved
    /// by mates. Default: the first component of an empty assembly is fixed (as in
    /// SolidWorks), any others float.
    #[serde(default)]
    pub fixed: Option<bool>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct PlacementParams {
    /// Where the component's own origin goes, in assembly millimeters. Default [0, 0, 0].
    #[serde(default)]
    pub origin_mm: Option<[f64; 3]>,
    /// The assembly direction the component's own X axis points along. Default: unturned
    /// (+X), or with only z_axis the smallest turn that brings Z there.
    #[serde(default)]
    pub x_axis: Option<[f64; 3]>,
    /// The assembly direction the component's own Z axis points along, perpendicular to
    /// x_axis. Default: unturned (+Z), or with only x_axis the smallest turn that brings X
    /// there.
    #[serde(default)]
    pub z_axis: Option<[f64; 3]>,
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn to_proto(v: DVec3) -> proto::Vec3 {
    proto::Vec3 { x: v.x, y: v.y, z: v.z }
}

fn direction(name: &str, v: Option<[f64; 3]>) -> Result<Option<DVec3>, McpError> {
    let Some(v) = v else { return Ok(None) };
    DVec3::from_array(v)
        .try_normalize()
        .filter(|d| d.is_finite())
        .map(Some)
        .ok_or_else(|| invalid(format!("{name} must be a non-zero direction like [0, 0, 1]")))
}

/// The component's origin and axes in assembly space (meters, unit,
/// right-handed).
fn placement(p: &PlacementParams) -> Result<proto::ComponentPlacement, McpError> {
    let origin = DVec3::from_array(p.origin_mm.unwrap_or_default());
    if !origin.is_finite() {
        return Err(invalid("origin_mm must be three finite numbers"));
    }
    let (x, z) = match (direction("x_axis", p.x_axis)?, direction("z_axis", p.z_axis)?) {
        (None, None) => (DVec3::X, DVec3::Z),
        (Some(x), None) => {
            let turn = DQuat::from_rotation_arc(DVec3::X, x);
            (x, turn * DVec3::Z)
        }
        (None, Some(z)) => {
            let turn = DQuat::from_rotation_arc(DVec3::Z, z);
            (turn * DVec3::X, z)
        }
        (Some(x), Some(z)) => {
            if x.dot(z).abs() > 1e-6 {
                return Err(invalid(format!(
                    "x_axis {:?} and z_axis {:?} must be perpendicular",
                    p.x_axis.unwrap_or_default(),
                    p.z_axis.unwrap_or_default()
                )));
            }
            ((x - x.dot(z) * z).normalize(), z)
        }
    };
    Ok(proto::ComponentPlacement {
        origin: Some(to_proto(origin / MM_PER_M)),
        x_axis: Some(to_proto(x)),
        y_axis: Some(to_proto(z.cross(x))),
        z_axis: Some(to_proto(z)),
    })
}

/// The InsertComponents request, with `copy_of` resolved by `resolve`
/// (the id registry: an id to a persist id).
pub fn request(
    params: &InsertComponentsParams,
    resolve: impl FnOnce(&str) -> Result<Vec<u8>, McpError>,
) -> Result<proto::InsertComponentsRequest, McpError> {
    use proto::insert_components_request::Source;
    let source = match (params.file.as_deref(), params.copy_of.as_deref().map(str::trim)) {
        (Some(_), Some(_)) => return Err(invalid("give file or copy_of, not both")),
        (None, None) => {
            return Err(invalid(
                "give the file to insert (an absolute .SLDPRT / .SLDASM path) or copy_of (a \
                 component id from inspect_model)",
            ));
        }
        (Some(file), None) => Source::Path(files::component_file(file)?),
        (None, Some(id)) => {
            if Kind::of_id(id) != Some(Kind::Component) {
                return Err(invalid(format!(
                    "copy_of takes a component id like \"component:2\" (inspect_model lists them), \
                     got {id:?}"
                )));
            }
            Source::CopyOfPersistId(resolve(id)?)
        }
    };
    let placements = match &params.at {
        None => vec![placement(&PlacementParams::default())?],
        Some(at) if at.is_empty() => return Err(invalid("`at` needs at least one placement")),
        Some(at) if at.len() > MAX_INSTANCES => {
            return Err(invalid(format!("at most {MAX_INSTANCES} components per call, got {}", at.len())));
        }
        Some(at) => at
            .iter()
            .enumerate()
            .map(|(i, p)| placement(p).map_err(|e| invalid(format!("at[{i}]: {}", e.message))))
            .collect::<Result<_, _>>()?,
    };
    Ok(proto::InsertComponentsRequest {
        source: Some(source),
        configuration: params.configuration.as_deref().map(str::trim).unwrap_or_default().to_string(),
        placements,
        fixed: params.fixed,
    })
}

pub fn reply_text(ids: &mut IdRegistry, reply: &proto::InsertComponentsResponse) -> String {
    let Some(first) = reply.components.first() else {
        return "inserted nothing.".into();
    };
    let file = first.file.rsplit(['\\', '/']).next().unwrap_or(&first.file);
    let what = if first.subassembly { "subassembly" } else { "component" };
    let mut text = format!(
        "inserted {} {what}{} of {file}",
        reply.components.len(),
        if reply.components.len() == 1 { "" } else { "s" }
    );
    if !first.configuration.is_empty() {
        text += &format!(" (configuration \"{}\")", first.configuration);
    }
    text += ":";
    for c in &reply.components {
        let id = ids.id_for(Kind::Component, &c.persist_id).unwrap_or_default();
        text += &format!("\n- {id}: {}", placement_text(c));
    }
    text += "\nMate them with add_mate: inspect_model what=\"faces\" or what=\"features\" \
             within=\"component:N\" gives a component's faces and planes.";
    text
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReplaceComponentsParams {
    /// The components to replace, by id ("component:3" from inspect_model what="components").
    /// Top-level components only (not ones inside a subassembly).
    pub components: Vec<String>,
    /// The new file, as an absolute path: a part (.SLDPRT) or an assembly (.SLDASM).
    pub file: String,
    /// true: also replace every other instance of the same file(s) in the assembly.
    /// Default false: only the components listed.
    #[serde(default)]
    pub all_instances: Option<bool>,
    /// The new file's configuration to use. Default: the one with the same name as the old
    /// component's, if the new file has it, else its active one.
    #[serde(default)]
    pub configuration: Option<String>,
}

/// The ReplaceComponents request, with the component ids resolved by
/// `resolve` (the id registry: an id to a persist id).
pub fn replace_request(
    params: &ReplaceComponentsParams,
    mut resolve: impl FnMut(&str) -> Result<Vec<u8>, McpError>,
) -> Result<proto::ReplaceComponentsRequest, McpError> {
    if params.components.is_empty() {
        return Err(invalid(
            "list the components to replace (\"component:N\" ids from inspect_model what=\"components\")",
        ));
    }
    if params.components.len() > MAX_INSTANCES {
        return Err(invalid(format!(
            "at most {MAX_INSTANCES} components per call, got {} (all_instances replaces every instance \
             of a file)",
            params.components.len()
        )));
    }
    let path = files::component_file(&params.file)?;
    let mut persist_ids = Vec::new();
    for id in params.components.iter().map(|id| id.trim()) {
        if Kind::of_id(id) != Some(Kind::Component) {
            return Err(invalid(format!(
                "components takes component ids like \"component:2\" (inspect_model lists them), got {id:?}"
            )));
        }
        persist_ids.push(resolve(id)?);
    }
    Ok(proto::ReplaceComponentsRequest {
        component_persist_ids: persist_ids,
        all_instances: params.all_instances.unwrap_or(false),
        path,
        configuration: params.configuration.as_deref().map(str::trim).unwrap_or_default().to_string(),
    })
}

fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

fn same_file(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn mate_failure(m: &proto::MateInfo) -> bool {
    m.error_code != 0 && !m.error_is_warning
}

pub fn replace_reply_text(
    ids: &mut IdRegistry,
    params: &ReplaceComponentsParams,
    reply: &proto::ReplaceComponentsResponse,
) -> String {
    let new_file = reply
        .replaced
        .iter()
        .filter_map(|r| r.after.as_ref().filter(|a| r.before.as_ref().is_some_and(|b| !same_file(&a.file, &b.file))))
        .chain(&reply.unpaired)
        .next()
        .map(|c| (file_name(&c.file).to_string(), c.configuration.clone()))
        .unwrap_or_else(|| (file_name(&params.file).to_string(), String::new()));

    // Each covered component: replaced, left alone, or lost track of.
    let mut done = Vec::new();
    let mut not_done = Vec::new();
    for r in &reply.replaced {
        let Some(before) = &r.before else { continue };
        let old = format!("{} ({})", before.name, file_name(&before.file));
        match &r.after {
            Some(after) if !same_file(&after.file, &before.file) => {
                let id = ids.id_for(Kind::Component, &after.persist_id).unwrap_or_default();
                done.push(format!("- {old} -> {id}: {}", placement_text(after)));
            }
            Some(_) => not_done.push(format!("{old}: SolidWorks left it as it was")),
            None => not_done.push(format!("{old}: can't be found any more")),
        }
    }
    let count = done.len() + reply.unpaired.len();
    let mut text = format!(
        "replaced {count} component{} with {}",
        if count == 1 { "" } else { "s" },
        new_file.0
    );
    if !new_file.1.is_empty() {
        text += &format!(" (configuration \"{}\")", new_file.1);
    }
    text += ":";
    for line in &done {
        text += &format!("\n{line}");
    }
    for c in &reply.unpaired {
        let id = ids.id_for(Kind::Component, &c.persist_id).unwrap_or_default();
        text += &format!("\n- {id} (new; which old component it replaced is unclear): {}", placement_text(c));
    }
    if !not_done.is_empty() {
        text += &format!("\nNot replaced: {}.", not_done.join("; "));
    }

    // Mates: which came through, which didn't.
    let (broken, kept): (Vec<_>, Vec<_>) = reply.mates.iter().partition(|m| m.broken);
    if reply.mates.is_empty() {
        text += "\nNo mates referenced the replaced components.";
    } else {
        text += &format!("\nMates: {} of {} re-attached", kept.len(), reply.mates.len());
        if !kept.is_empty() {
            let names: Vec<String> = kept
                .iter()
                .filter_map(|m| m.after.as_ref())
                .map(|a| {
                    let id = ids.id_for(Kind::Feature, &a.persist_id).unwrap_or_default();
                    format!("{} ({id})", a.name)
                })
                .collect();
            text += &format!(": {}", names.join(", "));
        }
        text.push('.');
    }
    if !broken.is_empty() {
        text += "\nUnresolved mates (give them new entities with edit_mate, or delete_features them):";
        for m in &broken {
            let Some(before) = &m.before else { continue };
            let was_broken = mate_failure(before);
            let Some(after) = &m.after else {
                text += &format!("\n- {} ({}): gone -- SolidWorks deleted it.", before.name, edit::mate_text(before));
                continue;
            };
            let id = ids.id_for(Kind::Feature, &after.persist_id).unwrap_or_default();
            text += &format!("\n- {} ({id}, {})", after.name, edit::mate_text(after));
            let entities: Vec<String> = m.entities.iter().map(|e| edit::entity_text(ids, e)).collect();
            if !entities.is_empty() {
                text += &format!(" between {}", entities.join(" and "));
            }
            let mut why = Vec::new();
            let missing = m.entities.iter().filter(|e| e.missing).count();
            if missing > 0 {
                let lost: Vec<&str> =
                    m.entities.iter().filter(|e| e.missing).map(|e| e.component_name.as_str()).filter(|n| !n.is_empty()).collect();
                let mut w = format!(
                    "{missing} reference{} not found in the new file",
                    if missing == 1 { "" } else { "s" }
                );
                if !lost.is_empty() {
                    w += &format!(" (on {})", lost.join(", "));
                }
                why.push(w);
            }
            if after.error_code != 0 {
                why.push(assembly::mate_error_name(after.error_code));
            }
            if !why.is_empty() {
                text += &format!(": {}", why.join("; "));
            }
            if was_broken {
                text += " (it already failed before the replacement)";
            }
            text.push('.');
        }
    }
    // The Mates folder warns whenever one of its mates fails: nothing new
    // when the broken mates are listed already.
    let problems: Vec<proto::FeatureProblem> = reply
        .new_errors
        .iter()
        .filter(|p| broken.is_empty() || p.type_name != "MateGroup")
        .cloned()
        .collect();
    let problems = delete::problems_text(&problems);
    if !problems.is_empty() {
        text += &format!("\n{}", problems.trim_start());
    }
    if count > 0 {
        text += "\nThe replaced components' faces, edges and planes have new ids: inspect_model \
                 within=\"component:N\" lists them.";
    }
    text
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FixComponentsParams {
    /// The components to fix or float, by id ("component:3" from inspect_model
    /// what="components"). Top-level components only (not ones inside a subassembly).
    pub components: Vec<String>,
    /// true: fix them in place (they can't move, mates move the others); false: float them,
    /// free to be moved by mates.
    pub fixed: bool,
}

/// The FixComponents request, with the component ids resolved by
/// `resolve` (the id registry: an id to a persist id).
pub fn fix_request(
    params: &FixComponentsParams,
    mut resolve: impl FnMut(&str) -> Result<Vec<u8>, McpError>,
) -> Result<proto::FixComponentsRequest, McpError> {
    if params.components.is_empty() {
        return Err(invalid(
            "list the components to fix or float (\"component:N\" ids from inspect_model what=\"components\")",
        ));
    }
    if params.components.len() > MAX_INSTANCES {
        return Err(invalid(format!(
            "at most {MAX_INSTANCES} components per call, got {}",
            params.components.len()
        )));
    }
    let mut persist_ids = Vec::new();
    for id in params.components.iter().map(|id| id.trim()) {
        if Kind::of_id(id) != Some(Kind::Component) {
            return Err(invalid(format!(
                "components takes component ids like \"component:2\" (inspect_model lists them), got {id:?}"
            )));
        }
        persist_ids.push(resolve(id)?);
    }
    Ok(proto::FixComponentsRequest { component_persist_ids: persist_ids, fixed: params.fixed })
}

/// Whether the rebuild moved the component (more than a micrometer, or
/// turned it).
fn moved(before: &proto::ComponentInfo, after: &proto::ComponentInfo) -> bool {
    let v = |v: &Option<proto::Vec3>| v.as_ref().map(|v| DVec3::new(v.x, v.y, v.z)).unwrap_or_default();
    [
        (&before.origin, &after.origin),
        (&before.x_axis, &after.x_axis),
        (&before.y_axis, &after.y_axis),
        (&before.z_axis, &after.z_axis),
    ]
    .iter()
    .any(|(a, b)| (v(a) - v(b)).length() > 1e-6)
}

pub fn fix_reply_text(ids: &mut IdRegistry, params: &FixComponentsParams, reply: &proto::FixComponentsResponse) -> String {
    let (verb, state) = if params.fixed { ("fixed", "fixed") } else { ("floated", "floating") };
    let mut changed = Vec::new();
    let mut already = Vec::new();
    let mut refused = Vec::new();
    let mut moved_any = false;
    for c in &reply.components {
        let (Some(before), Some(after)) = (&c.before, &c.after) else { continue };
        let id = ids.id_for(Kind::Component, &after.persist_id).unwrap_or_default();
        if before.fixed == params.fixed {
            already.push(format!("{} ({id})", after.name));
        } else if after.fixed != params.fixed {
            refused.push(format!("{} ({id})", after.name));
        } else {
            let mut line = format!("- {id}: {}", placement_text(after));
            if moved(before, after) {
                let was = before.origin.clone().unwrap_or_default();
                line += &format!(" (moved by the rebuild; its origin was at {} mm)", point_mm(&was));
                moved_any = true;
            }
            changed.push(line);
        }
    }
    let mut text = format!(
        "{verb} {} component{}",
        changed.len(),
        if changed.len() == 1 { "" } else { "s" }
    );
    text += if changed.is_empty() { "." } else { ":" };
    for line in &changed {
        text += &format!("\n{line}");
    }
    if !already.is_empty() {
        text += &format!("\nAlready {state}, left alone: {}.", already.join(", "));
    }
    if !refused.is_empty() {
        text += &format!("\nSolidWorks left these as they were: {}.", refused.join(", "));
    }
    if moved_any {
        text += "\nThe rebuild moved floated components to satisfy their mates: earlier inspect_model \
                 coordinates of them are out of date (their ids still work).";
    }
    let problems = delete::problems_text(&reply.new_errors);
    if !problems.is_empty() {
        text += &format!("\n{}", problems.trim_start());
        if params.fixed {
            text += "\nFixing can leave mates unsolvable (they'd have to move a fixed component): \
                     float it again, or edit_mate / delete_features the failing mates.";
        }
    }
    text
}

/// A turn of more than this per solver step risks the solver jumping to
/// another solution of the mates (a linkage flipping over).
const MAX_DEG_PER_STEP: f64 = 15.0;
const MAX_STEPS: u32 = 360;
/// Closer than this (mm, degrees) counts as the requested pose / no motion.
const REACHED_MM: f64 = 1e-3;
const REACHED_DEG: f64 = 0.01;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveComponentParams {
    /// The component to move, by id ("component:3" from inspect_model what="components"): a
    /// floating (not fixed) top-level component.
    pub component: String,
    /// How far to move it, [dx, dy, dz] in assembly millimeters; applied after the rotation.
    #[serde(default)]
    pub translate_mm: Option<[f64; 3]>,
    /// The direction of the axis to turn it about, in assembly coordinates, e.g. [0, 0, 1].
    /// Give with angle_deg.
    #[serde(default)]
    pub axis: Option<[f64; 3]>,
    /// How far to turn it about `axis`, in degrees (right-hand rule; negative turns the other
    /// way), -360..360.
    #[serde(default)]
    pub angle_deg: Option<f64>,
    /// A point the rotation axis goes through, in assembly millimeters. Default: the center
    /// of the component's bounding box (turn it in place).
    #[serde(default)]
    pub center_mm: Option<[f64; 3]>,
    /// The motion goes to the solver in this many equal parts, so a long turn follows its
    /// path (a linkage doesn't flip to another solution). Default: one per 15 degrees of
    /// turn, 1 for a translation. 1..360.
    #[serde(default)]
    pub steps: Option<u32>,
    /// true: afterwards put every component back where it started; the reply still says
    /// where the move took them, which makes it a "would the mates allow this" check.
    /// Default false: the move is kept.
    #[serde(default)]
    pub restore: bool,
    /// true: run an interference check before the move and after every step, and report
    /// the components that come to overlap on the way (first contact, how long, the biggest
    /// overlap). Use enough steps to see the contact closely (each check takes about half a
    /// second; the call stops after 45 s). Default false.
    #[serde(default)]
    pub check_collisions: bool,
    /// true (implies check_collisions): stop at the first step where components come to
    /// overlap and go back to the step before -- "move it until it hits something". Default
    /// false.
    #[serde(default)]
    pub stop_at_collision: bool,
}

/// The MoveComponent request, with the component id resolved by `resolve`.
pub fn move_request(
    params: &MoveComponentParams,
    resolve: impl FnOnce(&str) -> Result<Vec<u8>, McpError>,
) -> Result<proto::MoveComponentRequest, McpError> {
    let id = params.component.trim();
    if Kind::of_id(id) != Some(Kind::Component) {
        return Err(invalid(format!(
            "component takes a component id like \"component:2\" (inspect_model lists them), got {id:?}"
        )));
    }
    let translation = DVec3::from_array(params.translate_mm.unwrap_or_default());
    if !translation.is_finite() {
        return Err(invalid("translate_mm must be three finite numbers"));
    }
    let angle = params.angle_deg.unwrap_or_default();
    if !angle.is_finite() || angle.abs() > 360.0 {
        return Err(invalid(format!("angle_deg must be between -360 and 360, got {angle}")));
    }
    let axis = direction("axis", params.axis)?;
    let center = params.center_mm.map(DVec3::from_array);
    match (axis, angle != 0.0) {
        (None, true) => return Err(invalid("angle_deg needs the axis to turn about, e.g. axis=[0, 0, 1]")),
        (Some(_), false) => return Err(invalid("axis needs angle_deg, how far to turn")),
        _ => {}
    }
    if center.is_some() && angle == 0.0 {
        return Err(invalid("center_mm is the rotation axis' point: give it with axis and angle_deg"));
    }
    if center.is_some_and(|c| !c.is_finite()) {
        return Err(invalid("center_mm must be three finite numbers"));
    }
    if translation == DVec3::ZERO && angle == 0.0 {
        return Err(invalid("give translate_mm and / or axis + angle_deg: the motion to try"));
    }
    let steps = match params.steps {
        Some(n) if !(1..=MAX_STEPS).contains(&n) => {
            return Err(invalid(format!("steps must be 1..{MAX_STEPS}, got {n}")));
        }
        Some(n) => n,
        None => ((angle.abs() / MAX_DEG_PER_STEP).ceil() as u32).clamp(1, MAX_STEPS),
    };
    Ok(proto::MoveComponentRequest {
        component_persist_id: resolve(id)?,
        translation: Some(to_proto(translation / MM_PER_M)),
        rotation_axis: Some(to_proto(axis.unwrap_or(DVec3::Z))),
        rotation_angle: angle.to_radians(),
        rotation_center: center.map(|c| to_proto(c / MM_PER_M)),
        steps: steps as i32,
        restore: params.restore,
        check_collisions: params.check_collisions || params.stop_at_collision,
        stop_at_collision: params.stop_at_collision,
        time_budget: motion::TIME_BUDGET_S,
    })
}

fn vec(v: &Option<proto::Vec3>) -> DVec3 {
    v.as_ref().map(|v| DVec3::new(v.x, v.y, v.z)).unwrap_or_default()
}

/// Origin (meters) and rotation (the axes as columns).
fn frame(origin: &Option<proto::Vec3>, x: &Option<proto::Vec3>, y: &Option<proto::Vec3>, z: &Option<proto::Vec3>) -> (DVec3, DMat3) {
    (vec(origin), DMat3::from_cols(vec(x), vec(y), vec(z)))
}

fn info_frame(c: &proto::ComponentInfo) -> (DVec3, DMat3) {
    frame(&c.origin, &c.x_axis, &c.y_axis, &c.z_axis)
}

/// The turn taking `from` to `to`: angle in degrees (0..180) and axis.
fn turn(from: DMat3, to: DMat3) -> (f64, DVec3) {
    let (axis, angle) = DQuat::from_mat3(&(to * from.transpose())).normalize().to_axis_angle();
    let (axis, angle) = if angle > std::f64::consts::PI {
        (-axis, std::f64::consts::TAU - angle)
    } else {
        (axis, angle)
    };
    (angle.to_degrees(), axis)
}

/// "origin [..] -> [..] mm (by [..] mm), turned 20.0 deg about [..]".
fn motion_text(from: (DVec3, DMat3), to: (DVec3, DMat3)) -> String {
    let point = |p: DVec3| point_mm(&to_proto(p));
    let mut text = format!(
        "origin {} -> {} mm (by {} mm, {} mm)",
        point(from.0),
        point(to.0),
        point(to.0 - from.0),
        mm((to.0 - from.0).length())
    );
    let (deg, axis) = turn(from.1, to.1);
    if deg >= REACHED_DEG {
        text += &format!(", turned {} deg about {}", round(deg, 2), unit_text(&to_proto(axis)));
    }
    text
}

/// Whether `from` -> `to` is a motion at all: the solver leaves sub-micron
/// noise on components it didn't really move (seen 2026-10-25).
fn moves(from: (DVec3, DMat3), to: (DVec3, DMat3)) -> bool {
    (to.0 - from.0).length() * MM_PER_M >= REACHED_MM || turn(from.1, to.1).0 >= REACHED_DEG
}

pub fn move_reply_text(ids: &mut IdRegistry, params: &MoveComponentParams, request: &proto::MoveComponentRequest, reply: &proto::MoveComponentResponse) -> String {
    let (Some(before), Some(after)) = reply.component.as_ref().map(|c| (c.before.as_ref(), c.after.as_ref())).unwrap_or_default() else {
        return "SolidWorks didn't report the component".into();
    };
    let id = ids.id_for(Kind::Component, &after.persist_id).unwrap_or_default();
    let start = info_frame(before);
    let reached = info_frame(after);
    let target = reply.target.as_ref().map(|t| frame(&t.origin, &t.x_axis, &t.y_axis, &t.z_axis)).unwrap_or(start);

    let mut text = if params.restore {
        format!("Tried moving {} ({id}), then put everything back.", after.name)
    } else {
        format!("Moved {} ({id}).", after.name)
    };
    text += &format!("\nAsked: {}", motion_text(start, target));
    if request.rotation_angle != 0.0 {
        text += &format!(
            " (axis through {} mm, in {} step{})",
            point_mm(&reply.rotation_center.clone().unwrap_or_default()),
            request.steps,
            if request.steps == 1 { "" } else { "s" }
        );
    }
    text += &format!("\nGot:   {}", motion_text(start, reached));

    // How far it got: off from the target, and how much it moved at all.
    let off_mm = (target.0 - reached.0).length() * MM_PER_M;
    let off_deg = turn(reached.1, target.1).0;
    text += "\nSolver: ";
    text += &if off_mm < REACHED_MM && off_deg < REACHED_DEG {
        "reached the requested pose (its mates allow this motion).".to_string()
    } else if !moves(start, reached) {
        "it didn't move: its mates don't allow this motion (or the solver wouldn't move what \
         they tie it to). inspect_model what=\"mates\" shows what holds it."
            .to_string()
    } else {
        format!(
            "its mates held it back: it ended {} mm and {} deg from the requested pose, having \
             moved along the directions they leave free.",
            mm(off_mm / MM_PER_M),
            round(off_deg, 2)
        )
    };
    if reply.refused_step > 0 {
        text += &format!(
            "\nSolidWorks refused step {} of {}; the move stopped there.",
            reply.refused_step, request.steps
        );
    }

    let others: Vec<_> = reply
        .others
        .iter()
        .filter_map(|o| Some((o.before.as_ref()?, o.after.as_ref()?)))
        .filter(|(b, a)| moves(info_frame(b), info_frame(a)))
        .collect();
    if !others.is_empty() {
        text += "\nAlso moved by their mates:";
        for (b, a) in &others {
            let other_id = ids.id_for(Kind::Component, &a.persist_id).unwrap_or_default();
            text += &format!("\n- {} ({other_id}): {}", a.name, motion_text(info_frame(b), info_frame(a)));
        }
    }

    if request.check_collisions {
        text += &motion::collisions_text(
            ids,
            &reply.start_interferences,
            &reply.collision_steps,
            |s| motion::move_step_label(request, s),
            "before the move",
        );
        if reply.backed_off {
            text += &if reply.collision_step <= 1 {
                "\nStopped at the first collision (step 1) and went back to where everything started: \
                 it can't move that way at all without hitting something (a smaller step would get closer)."
                    .to_string()
            } else {
                format!(
                    "\nStopped at the first collision (step {} of {}) and went back to step {}'s pose: \
                     \"Got\" above is that pose.",
                    reply.collision_step,
                    request.steps,
                    reply.collision_step - 1
                )
            };
        }
        if reply.out_of_time {
            text += &format!(
                "\nOut of time after step {} of {}: the move stopped there (fewer steps, or check a \
                 shorter motion).",
                reply.collision_steps.last().map_or(0, |s| s.step),
                request.steps
            );
        }
    }

    let problems = delete::problems_text(&reply.new_errors);
    if !problems.is_empty() {
        text += &format!("\nAfter the move: {}", problems.trim_start());
        if !params.restore {
            text += "\nMove it back (the opposite motion), or edit_mate / delete_features the failing mates.";
        }
    }

    if params.restore {
        if reply.restored {
            text += "\nRestored: every component is back where it started (the poses above are where \
                     the move took them).";
        } else {
            text += &format!(
                "\nNot back where they started: {} -- they're where the move left them.",
                reply.not_restored.join(", ")
            );
        }
    } else if moves(start, reached) || !others.is_empty() {
        text += "\nEarlier inspect_model coordinates of the moved components are out of date (their ids \
                 still work).";
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(v: &Option<proto::Vec3>, want: [f64; 3]) -> bool {
        let v = v.as_ref().unwrap();
        (v.x - want[0]).abs() < 1e-12 && (v.y - want[1]).abs() < 1e-12 && (v.z - want[2]).abs() < 1e-12
    }

    fn at(origin: [f64; 3], x: Option<[f64; 3]>, z: Option<[f64; 3]>) -> PlacementParams {
        PlacementParams { origin_mm: Some(origin), x_axis: x, z_axis: z }
    }

    #[test]
    fn default_placement_is_the_origin_unturned() {
        let p = placement(&PlacementParams::default()).unwrap();
        assert!(close(&p.origin, [0.0; 3]));
        assert!(close(&p.x_axis, [1.0, 0.0, 0.0]));
        assert!(close(&p.y_axis, [0.0, 1.0, 0.0]));
        assert!(close(&p.z_axis, [0.0, 0.0, 1.0]));
    }

    #[test]
    fn origin_in_meters() {
        let p = placement(&at([10.0, -20.0, 5.0], None, None)).unwrap();
        assert!(close(&p.origin, [0.01, -0.02, 0.005]));
    }

    #[test]
    fn one_axis_turns_the_least() {
        // X along +Y: a quarter turn about Z.
        let p = placement(&at([0.0; 3], Some([0.0, 2.0, 0.0]), None)).unwrap();
        assert!(close(&p.x_axis, [0.0, 1.0, 0.0]));
        assert!(close(&p.y_axis, [-1.0, 0.0, 0.0]));
        assert!(close(&p.z_axis, [0.0, 0.0, 1.0]));
        // Z upside down: some half turn, still right-handed.
        let p = placement(&at([0.0; 3], None, Some([0.0, 0.0, -1.0]))).unwrap();
        let (x, y, z) = (p.x_axis.unwrap(), p.y_axis.unwrap(), p.z_axis.unwrap());
        let (x, y, z) = (DVec3::new(x.x, x.y, x.z), DVec3::new(y.x, y.y, y.z), DVec3::new(z.x, z.y, z.z));
        assert!((z - DVec3::NEG_Z).length() < 1e-12);
        assert!((x.cross(y) - z).length() < 1e-12 && x.dot(z).abs() < 1e-12);
    }

    #[test]
    fn both_axes_must_be_perpendicular() {
        // Right-handed: y = z cross x.
        let p = placement(&at([0.0; 3], Some([0.0, 0.0, 1.0]), Some([1.0, 0.0, 0.0]))).unwrap();
        assert!(close(&p.y_axis, [0.0, -1.0, 0.0]));
        assert!(placement(&at([0.0; 3], Some([1.0, 1.0, 0.0]), Some([1.0, 0.0, 0.0]))).is_err());
        assert!(placement(&at([0.0; 3], Some([0.0, 0.0, 0.0]), None)).is_err());
    }

    fn params(file: Option<&str>, copy_of: Option<&str>) -> InsertComponentsParams {
        InsertComponentsParams {
            file: file.map(Into::into),
            copy_of: copy_of.map(Into::into),
            configuration: None,
            at: None,
            fixed: None,
        }
    }

    #[test]
    fn source_is_a_file_or_a_component() {
        let none = |_: &str| -> Result<Vec<u8>, McpError> { panic!("not resolved") };
        assert!(request(&params(None, None), none).is_err());
        assert!(request(&params(Some("C:\\a.SLDPRT"), Some("component:1")), none).is_err());
        assert!(request(&params(Some("C:\\a\\b.SLDDRW"), None), none).is_err());
        assert!(request(&params(Some("b.SLDPRT"), None), none).is_err());
        assert!(request(&params(None, Some("face:3")), none).is_err());
        let r = request(&params(None, Some("component:3")), |id| Ok(id.as_bytes().to_vec())).unwrap();
        assert_eq!(
            r.source,
            Some(proto::insert_components_request::Source::CopyOfPersistId(b"component:3".to_vec()))
        );
        assert_eq!(r.placements.len(), 1);
        assert_eq!(r.fixed, None);
    }

    #[test]
    fn several_placements_several_components() {
        let mut p = params(None, Some("component:3"));
        p.at = Some(vec![at([0.0; 3], None, None), at([30.0, 0.0, 0.0], None, None)]);
        p.fixed = Some(false);
        let r = request(&p, |_| Ok(vec![1])).unwrap();
        assert_eq!(r.placements.len(), 2);
        assert!(close(&r.placements[1].origin, [0.03, 0.0, 0.0]));
        assert_eq!(r.fixed, Some(false));
        p.at = Some(vec![]);
        assert!(request(&p, |_| Ok(vec![1])).is_err());
        p.at = Some(vec![at([0.0; 3], Some([0.0, 0.0, 0.0]), None)]);
        let e = request(&p, |_| Ok(vec![1])).unwrap_err();
        assert!(e.message.starts_with("at[0]: "), "{}", e.message);
    }

    fn replace_params(components: &[&str], file: &str) -> ReplaceComponentsParams {
        ReplaceComponentsParams {
            components: components.iter().map(|s| s.to_string()).collect(),
            file: file.into(),
            all_instances: None,
            configuration: None,
        }
    }

    #[test]
    fn replace_takes_component_ids_and_a_file() {
        let file = std::env::current_exe().unwrap().with_file_name("replace-test.SLDPRT");
        std::fs::write(&file, b"").unwrap();
        let file = file.to_str().unwrap();
        let resolve = |id: &str| -> Result<Vec<u8>, McpError> { Ok(id.as_bytes().to_vec()) };
        assert!(replace_request(&replace_params(&[], file), resolve).is_err());
        assert!(replace_request(&replace_params(&["face:1"], file), resolve).is_err());
        assert!(replace_request(&replace_params(&["component:1"], "C:\\nope\\x.SLDPRT"), resolve).is_err());
        assert!(replace_request(&replace_params(&["component:1"], "x.SLDPRT"), resolve).is_err());
        let mut p = replace_params(&["component:1", " component:2 "], file);
        p.configuration = Some(" Long ".into());
        let r = replace_request(&p, resolve).unwrap();
        assert_eq!(r.component_persist_ids, vec![b"component:1".to_vec(), b"component:2".to_vec()]);
        assert_eq!((r.all_instances, r.configuration.as_str()), (false, "Long"));
        std::fs::remove_file(file).ok();
    }

    fn component(pid: &[u8], name: &str, file: &str) -> proto::ComponentInfo {
        proto::ComponentInfo { persist_id: pid.to_vec(), name: name.into(), file: file.into(), ..Default::default() }
    }

    fn mate(pid: &[u8], name: &str, error: i32) -> proto::MateInfo {
        proto::MateInfo {
            persist_id: pid.to_vec(),
            name: name.into(),
            r#type: proto::MateType::Coincident.into(),
            error_code: error,
            ..Default::default()
        }
    }

    #[test]
    fn replace_text_reports_components_and_unresolved_mates() {
        let mut ids = IdRegistry::default();
        let reply = proto::ReplaceComponentsResponse {
            replaced: vec![
                proto::ComponentReplacement {
                    before: Some(component(b"a", "block-1", "C:\\d\\block.SLDPRT")),
                    after: Some(component(b"a2", "plate-1", "C:\\d\\plate.SLDPRT")),
                },
                proto::ComponentReplacement {
                    before: Some(component(b"b", "block-2", "C:\\d\\block.SLDPRT")),
                    after: Some(component(b"b", "block-2", "C:\\d\\block.SLDPRT")),
                },
            ],
            unpaired: vec![],
            mates: vec![
                proto::ReplacedMate {
                    before: Some(mate(b"m1", "Coincident1", 0)),
                    after: Some(mate(b"m1", "Coincident1", 0)),
                    entities: vec![],
                    broken: false,
                },
                proto::ReplacedMate {
                    before: Some(mate(b"m2", "Coincident2", 0)),
                    after: Some(mate(b"m2", "Coincident2", 48)),
                    entities: vec![
                        proto::MateEntityState { missing: true, component_name: "plate-1".into(), ..Default::default() },
                        proto::MateEntityState {
                            kind: "face".into(),
                            persist_id: b"f".to_vec(),
                            component_name: "base-1".into(),
                            ..Default::default()
                        },
                    ],
                    broken: true,
                },
                proto::ReplacedMate { before: Some(mate(b"m3", "Coincident3", 0)), after: None, entities: vec![], broken: true },
            ],
            new_errors: vec![
                proto::FeatureProblem { name: "Mates".into(), type_name: "MateGroup".into(), error_code: 1, warning: true },
                proto::FeatureProblem { name: "LPattern1".into(), type_name: "LPattern".into(), error_code: 73, warning: false },
            ],
        };
        let params = replace_params(&["component:1", "component:2"], "C:\\d\\plate.SLDPRT");
        let text = replace_reply_text(&mut ids, &params, &reply);
        assert!(text.starts_with("replaced 1 component with plate.SLDPRT:"), "{text}");
        assert!(text.contains("- block-1 (block.SLDPRT) -> component:"), "{text}");
        assert!(text.contains("Not replaced: block-2 (block.SLDPRT): SolidWorks left it as it was."), "{text}");
        assert!(text.contains("Mates: 1 of 3 re-attached: Coincident1 (feature:"), "{text}");
        assert!(text.contains("- Coincident2 (feature:"), "{text}");
        assert!(text.contains("between (missing) and face:1 on base-1"), "{text}");
        assert!(text.contains("1 reference not found in the new file (on plate-1); a mated entity is missing"), "{text}");
        assert!(text.contains("- Coincident3 (coincident): gone -- SolidWorks deleted it.\n"), "{text}");
        assert!(text.contains("a mated entity is missing.\n"), "{text}");
        assert!(text.contains("\nNow failing to rebuild: LPattern1"), "{text}");
        assert!(!text.contains("Mates (") && !text.contains("warnings"), "{text}");
    }

    #[test]
    fn fix_takes_component_ids() {
        let resolve = |id: &str| -> Result<Vec<u8>, McpError> { Ok(id.as_bytes().to_vec()) };
        let p = |components: &[&str], fixed| FixComponentsParams {
            components: components.iter().map(|s| s.to_string()).collect(),
            fixed,
        };
        assert!(fix_request(&p(&[], true), resolve).is_err());
        assert!(fix_request(&p(&["face:1"], true), resolve).is_err());
        let r = fix_request(&p(&[" component:1 ", "component:2"], false), resolve).unwrap();
        assert_eq!(r.component_persist_ids, vec![b"component:1".to_vec(), b"component:2".to_vec()]);
        assert!(!r.fixed);
    }

    fn placed(pid: &[u8], name: &str, fixed: bool, origin: [f64; 3]) -> proto::ComponentInfo {
        proto::ComponentInfo {
            persist_id: pid.to_vec(),
            name: name.into(),
            fixed,
            origin: Some(proto::Vec3 { x: origin[0], y: origin[1], z: origin[2] }),
            ..Default::default()
        }
    }

    #[test]
    fn fix_text_reports_changed_moved_and_untouched() {
        let mut ids = IdRegistry::default();
        let state = |before, after| proto::ComponentFixState { before: Some(before), after: Some(after) };
        let reply = proto::FixComponentsResponse {
            components: vec![
                state(placed(b"a", "block-1", true, [0.0; 3]), placed(b"a", "block-1", false, [0.0; 3])),
                state(placed(b"b", "block-2", true, [0.0; 3]), placed(b"b", "block-2", false, [0.01, 0.0, 0.0])),
                state(placed(b"c", "block-3", false, [0.0; 3]), placed(b"c", "block-3", false, [0.0; 3])),
                state(placed(b"d", "block-4", true, [0.0; 3]), placed(b"d", "block-4", true, [0.0; 3])),
            ],
            new_errors: vec![],
        };
        let params = FixComponentsParams { components: vec![], fixed: false };
        let text = fix_reply_text(&mut ids, &params, &reply);
        assert!(text.starts_with("floated 2 components:\n- component:1: block-1 has its origin"), "{text}");
        assert!(!text.lines().nth(1).unwrap().contains("moved"), "{text}");
        assert!(text.contains("block-2 has its origin at [10.0,0.0,0.0] mm (moved by the rebuild; its origin was at [0.0,0.0,0.0] mm)"), "{text}");
        assert!(text.contains("\nAlready floating, left alone: block-3 (component:3)."), "{text}");
        assert!(text.contains("\nSolidWorks left these as they were: block-4 (component:4)."), "{text}");
        assert!(text.contains("out of date"), "{text}");

        let reply = proto::FixComponentsResponse {
            components: vec![state(placed(b"a", "block-1", false, [0.0; 3]), placed(b"a", "block-1", true, [0.0; 3]))],
            new_errors: vec![proto::FeatureProblem {
                name: "Distance1".into(),
                type_name: "MateDistanceDim".into(),
                error_code: 46,
                warning: false,
            }],
        };
        let params = FixComponentsParams { components: vec![], fixed: true };
        let text = fix_reply_text(&mut ids, &params, &reply);
        assert!(text.starts_with("fixed 1 component:\n- component:1: block-1 (fixed)"), "{text}");
        assert!(text.contains("Now failing to rebuild: Distance1"), "{text}");
        assert!(text.contains("float it again"), "{text}");
    }

    fn move_params(translate: Option<[f64; 3]>, axis: Option<[f64; 3]>, angle: Option<f64>) -> MoveComponentParams {
        MoveComponentParams {
            component: "component:1".into(),
            translate_mm: translate,
            axis,
            angle_deg: angle,
            center_mm: None,
            steps: None,
            restore: false,
            check_collisions: false,
            stop_at_collision: false,
        }
    }

    #[test]
    fn move_request_checks_the_motion() {
        let resolve = |id: &str| -> Result<Vec<u8>, McpError> { Ok(id.as_bytes().to_vec()) };
        assert!(move_request(&move_params(None, None, None), resolve).is_err());
        assert!(move_request(&move_params(Some([0.0; 3]), None, None), resolve).is_err());
        assert!(move_request(&move_params(None, None, Some(30.0)), resolve).is_err());
        assert!(move_request(&move_params(None, Some([0.0, 0.0, 1.0]), None), resolve).is_err());
        assert!(move_request(&move_params(None, Some([0.0; 3]), Some(30.0)), resolve).is_err());
        assert!(move_request(&move_params(None, Some([0.0, 0.0, 1.0]), Some(400.0)), resolve).is_err());
        let mut p = move_params(Some([10.0, 0.0, 0.0]), None, None);
        p.center_mm = Some([0.0; 3]);
        assert!(move_request(&p, resolve).is_err(), "center without a rotation");
        p.center_mm = None;
        p.component = "face:1".into();
        assert!(move_request(&p, resolve).is_err());
        p.component = "component:1".into();
        p.steps = Some(0);
        assert!(move_request(&p, resolve).is_err());

        let r = move_request(&move_params(Some([10.0, 0.0, 0.0]), None, None), resolve).unwrap();
        assert_eq!(r.translation.unwrap().x, 0.01);
        assert_eq!((r.rotation_angle, r.steps, r.rotation_center), (0.0, 1, None));
        let mut p = move_params(None, Some([0.0, 0.0, 2.0]), Some(-100.0));
        p.center_mm = Some([5.0, 0.0, 0.0]);
        let r = move_request(&p, resolve).unwrap();
        assert_eq!(r.steps, 7, "one per 15 degrees");
        assert!((r.rotation_angle + 100f64.to_radians()).abs() < 1e-12);
        assert_eq!(r.rotation_axis.unwrap().z, 1.0);
        assert_eq!(r.rotation_center.unwrap().x, 0.005);
    }

    fn pose(pid: &[u8], name: &str, origin: [f64; 3], turned_deg: f64) -> proto::ComponentInfo {
        let (s, c) = turned_deg.to_radians().sin_cos();
        let v = |x, y, z| Some(proto::Vec3 { x, y, z });
        proto::ComponentInfo {
            persist_id: pid.to_vec(),
            name: name.into(),
            origin: v(origin[0], origin[1], origin[2]),
            x_axis: v(c, s, 0.0),
            y_axis: v(-s, c, 0.0),
            z_axis: v(0.0, 0.0, 1.0),
            ..Default::default()
        }
    }

    fn move_reply(after: proto::ComponentInfo, target: &proto::ComponentInfo) -> proto::MoveComponentResponse {
        proto::MoveComponentResponse {
            component: Some(proto::ComponentMove { before: Some(pose(b"s", "slider-1", [0.0, 0.06, 0.0], 0.0)), after: Some(after) }),
            target: Some(proto::ComponentPlacement {
                origin: target.origin.clone(),
                x_axis: target.x_axis.clone(),
                y_axis: target.y_axis.clone(),
                z_axis: target.z_axis.clone(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn move_text_tells_how_far_it_got() {
        let resolve = |id: &str| -> Result<Vec<u8>, McpError> { Ok(id.as_bytes().to_vec()) };
        let mut ids = IdRegistry::default();
        let params = move_params(Some([10.0, 5.0, 3.0]), None, None);
        let request = move_request(&params, resolve).unwrap();
        let target = pose(b"s", "slider-1", [0.01, 0.065, 0.003], 0.0);

        // Held back: only x was free; the follower came along.
        let mut reply = move_reply(pose(b"s", "slider-1", [0.01, 0.06, 0.0], 0.0), &target);
        reply.others.push(proto::ComponentMove {
            before: Some(pose(b"f", "follower-1", [0.0, 0.12, 0.0], 0.0)),
            after: Some(pose(b"f", "follower-1", [0.01, 0.12, 0.0], 0.0)),
        });
        let text = move_reply_text(&mut ids, &params, &request, &reply);
        assert!(text.starts_with("Moved slider-1 (component:1).\nAsked: origin [0.0,60.0,0.0] -> [10.0,65.0,3.0] mm"), "{text}");
        assert!(text.contains("\nGot:   origin [0.0,60.0,0.0] -> [10.0,60.0,0.0] mm (by [10.0,0.0,0.0] mm, 10 mm)"), "{text}");
        assert!(text.contains("held it back: it ended 5.831 mm and 0 deg from"), "{text}");
        assert!(text.contains("\n- follower-1 (component:2): origin [0.0,120.0,0.0] -> [10.0,120.0,0.0] mm"), "{text}");
        assert!(text.contains("out of date"), "{text}");

        // Reached.
        let text = move_reply_text(&mut ids, &params, &request, &move_reply(target.clone(), &target));
        assert!(text.contains("Solver: reached the requested pose"), "{text}");

        // Didn't move, with restore.
        let mut params = params;
        params.restore = true;
        let mut reply = move_reply(pose(b"s", "slider-1", [0.0, 0.06, 0.0], 0.0), &target);
        reply.restored = true;
        reply.others.push(proto::ComponentMove {
            before: Some(pose(b"f", "follower-1", [0.0, 0.12, 0.0], 0.0)),
            after: Some(pose(b"f", "follower-1", [1e-9, 0.12, 0.0], 1e-6)),
        });
        let text = move_reply_text(&mut ids, &params, &request, &reply);
        assert!(text.starts_with("Tried moving slider-1 (component:1), then put everything back."), "{text}");
        assert!(text.contains("Solver: it didn't move"), "{text}");
        assert!(text.contains("Restored: every component is back") && !text.contains("out of date"), "{text}");
        assert!(!text.contains("Also moved"), "solver noise isn't a move: {text}");

        // A turn: reported in degrees about the axis.
        let params = move_params(None, Some([0.0, 0.0, 1.0]), Some(30.0));
        let request = move_request(&params, resolve).unwrap();
        let turned = pose(b"s", "slider-1", [0.0, 0.06, 0.0], 30.0);
        let mut reply = move_reply(turned.clone(), &turned);
        reply.new_errors.push(proto::FeatureProblem { name: "Angle1".into(), type_name: "MateAngleDim".into(), error_code: 46, warning: false });
        let text = move_reply_text(&mut ids, &params, &request, &reply);
        assert!(text.contains("turned 30 deg about [0.0,0.0,1.0] (axis through [0.0,0.0,0.0] mm, in 2 steps)"), "{text}");
        assert!(text.contains("Now failing to rebuild: Angle1") && text.contains("Move it back"), "{text}");
    }
}

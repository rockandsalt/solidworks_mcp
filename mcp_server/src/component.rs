//! Inserting components into an assembly: the insert_components tool's
//! params, turning each placement (origin + optional axes) into the
//! orthonormal frame the add-in hands to AddComponents3, and the reply.
//!
//! Parts and subassemblies go in the same way (a subassembly is just an
//! .SLDASM file); several instances of one file are several placements, and
//! `copy_of` takes the file and configuration from a component already in
//! the assembly.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use glam::{DQuat, DVec3};

use crate::addin::proto;
use crate::assembly::placement_text;
use crate::files;
use crate::inspect::{IdRegistry, Kind, MM_PER_M};

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
}

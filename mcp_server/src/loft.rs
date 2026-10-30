//! The loft tools (loft_sketch, cut_loft_sketch): their parameters and the
//! reply text.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::Kind;

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoftParams {
    /// Feature-tree names of the profile sketches, in the order the loft passes through them,
    /// e.g. ["Sketch1", "Sketch2"] (at least 2). Each is one closed contour on its own plane,
    /// e.g. Front and a create_reference_plane offset from it; the first or the last may
    /// instead be a sketch holding a single sketch_point, to loft to a point (a pyramid, a
    /// cone, a tip).
    pub profiles: Vec<String>,
    /// Feature-tree names of guide curves that shape the loft between the profiles, e.g.
    /// ["Sketch3"]: sketches (or a 3D sketch or curve) each touching every profile, usually at
    /// a corner or on an edge. Defaults to none: the loft blends smoothly from profile to
    /// profile.
    #[serde(default)]
    pub guide_curves: Vec<String>,
    /// Loft from the last profile back to the first, making a ring (at least 3 profiles,
    /// usually on planes round an axis). Defaults to false.
    #[serde(default)]
    pub closed: bool,
    /// Leave the first profile at right angles to it (start tangency "normal to profile"),
    /// e.g. so the loft continues a straight extrusion smoothly. Not with closed. Defaults to
    /// false.
    #[serde(default)]
    pub start_normal: bool,
    /// Reach the last profile at right angles to it (end tangency "normal to profile"). Not
    /// with closed. Defaults to false.
    #[serde(default)]
    pub end_normal: bool,
}

/// More than any loft needs (the add-in's limits).
const MAX_PROFILES: usize = 100;
const MAX_GUIDES: usize = 20;

/// A trimmed feature-tree name, refusing ids such as "feature:3".
fn feature_name(what: &str, name: &str) -> Result<String, McpError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(invalid(format!("{what}: a name is empty -- give feature-tree names such as \"Sketch1\"")));
    }
    if Kind::of_id(name).is_some() {
        return Err(invalid(format!(
            "{what}: give the feature-tree name of a sketch (e.g. \"Sketch1\"), not an id like {name}"
        )));
    }
    Ok(name.to_string())
}

pub fn request(params: &LoftParams, cut: bool) -> Result<proto::LoftSketchRequest, McpError> {
    let profiles = params
        .profiles
        .iter()
        .map(|p| feature_name("profiles", p))
        .collect::<Result<Vec<_>, _>>()?;
    let guide_curves = params
        .guide_curves
        .iter()
        .map(|g| feature_name("guide_curves", g))
        .collect::<Result<Vec<_>, _>>()?;
    if profiles.len() < 2 {
        return Err(invalid(format!(
            "a loft needs at least 2 profiles, got {} -- for one sketch use extrude_sketch",
            profiles.len()
        )));
    }
    if profiles.len() > MAX_PROFILES {
        return Err(invalid(format!("at most {MAX_PROFILES} profiles, got {}", profiles.len())));
    }
    if guide_curves.len() > MAX_GUIDES {
        return Err(invalid(format!("at most {MAX_GUIDES} guide curves, got {}", guide_curves.len())));
    }
    let mut all: Vec<&String> = profiles.iter().chain(&guide_curves).collect();
    all.sort();
    if let Some(twice) = all.windows(2).find(|w| w[0] == w[1]) {
        return Err(invalid(format!(
            "{} is given more than once -- each profile and guide curve must be its own sketch",
            twice[0]
        )));
    }
    if params.closed && profiles.len() < 3 {
        return Err(invalid("a closed loft needs at least 3 profiles"));
    }
    if params.closed && (params.start_normal || params.end_normal) {
        return Err(invalid("a closed loft has no start or end -- drop start_normal / end_normal"));
    }
    Ok(proto::LoftSketchRequest {
        profiles,
        guide_curves,
        closed: params.closed,
        start_normal: params.start_normal,
        end_normal: params.end_normal,
        cut,
    })
}

/// "A, B and C"
fn list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// "created Loft1 by lofting through Sketch1, Sketch2 and Sketch3 along guide curve Sketch4."
pub fn reply_text(request: &proto::LoftSketchRequest, reply: &proto::LoftSketchResponse) -> String {
    let guides = match request.guide_curves.len() {
        0 => String::new(),
        1 => format!(" along guide curve {}", request.guide_curves[0]),
        _ => format!(" along guide curves {}", list(&request.guide_curves)),
    };
    let closed = if request.closed { format!(", closed back to {}", request.profiles[0]) } else { String::new() };
    let tangency = match (request.start_normal, request.end_normal) {
        (true, true) => ", normal to the first and last profiles",
        (true, false) => ", normal to the first profile",
        (false, true) => ", normal to the last profile",
        (false, false) => "",
    };
    format!(
        "created {} by {} through {}{guides}{closed}{tangency}.",
        reply.feature_name,
        if request.cut { "cutting a loft" } else { "lofting" },
        list(&request.profiles),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(profiles: &[&str]) -> LoftParams {
        LoftParams {
            profiles: profiles.iter().map(|p| p.to_string()).collect(),
            guide_curves: Vec::new(),
            closed: false,
            start_normal: false,
            end_normal: false,
        }
    }

    #[test]
    fn names_trimmed_in_order() {
        let mut p = params(&[" Sketch2 ", "Sketch1"]);
        p.guide_curves = vec!["Sketch3 ".into()];
        let r = request(&p, true).unwrap();
        assert_eq!(r.profiles, ["Sketch2", "Sketch1"]);
        assert_eq!(r.guide_curves, ["Sketch3"]);
        assert!(r.cut && !r.closed);
    }

    #[test]
    fn needs_two_distinct_named_profiles() {
        assert!(request(&params(&["Sketch1"]), false).is_err());
        assert!(request(&params(&["Sketch1", " "]), false).is_err());
        let err = request(&params(&["Sketch1", "feature:2"]), false).unwrap_err();
        assert!(err.message.contains("feature-tree name"), "{}", err.message);
        let err = request(&params(&["Sketch1", "Sketch1 "]), false).unwrap_err();
        assert!(err.message.contains("Sketch1 is given more than once"), "{}", err.message);
        let mut p = params(&["Sketch1", "Sketch2"]);
        p.guide_curves = vec!["Sketch2".into()];
        assert!(request(&p, false).is_err());
    }

    #[test]
    fn closed_needs_three_and_no_tangency() {
        let mut p = params(&["Sketch1", "Sketch2"]);
        p.closed = true;
        assert!(request(&p, false).is_err());
        p.profiles.push("Sketch3".into());
        assert!(request(&p, false).unwrap().closed);
        p.end_normal = true;
        assert!(request(&p, false).is_err());
    }

    #[test]
    fn reply_names_profiles_guides_and_options() {
        let reply = proto::LoftSketchResponse { feature_name: "Loft1".into(), ..Default::default() };
        let mut p = params(&["Sketch1", "Sketch2", "Sketch3"]);
        p.guide_curves = vec!["Sketch4".into()];
        assert_eq!(
            reply_text(&request(&p, false).unwrap(), &reply),
            "created Loft1 by lofting through Sketch1, Sketch2 and Sketch3 along guide curve Sketch4."
        );
        p.guide_curves.clear();
        p.closed = true;
        let reply = proto::LoftSketchResponse { feature_name: "Cut-Loft1".into(), ..Default::default() };
        assert_eq!(
            reply_text(&request(&p, true).unwrap(), &reply),
            "created Cut-Loft1 by cutting a loft through Sketch1, Sketch2 and Sketch3, closed back to Sketch1."
        );
        let mut p = params(&["Sketch1", "Sketch2"]);
        p.start_normal = true;
        p.end_normal = true;
        let reply = proto::LoftSketchResponse { feature_name: "Loft2".into(), ..Default::default() };
        assert_eq!(
            reply_text(&request(&p, false).unwrap(), &reply),
            "created Loft2 by lofting through Sketch1 and Sketch2, normal to the first and last profiles."
        );
    }
}

//! Text the feature tools share: SolidWorks feature types and rebuild
//! errors in words, the features a change broke, and numbered request items
//! named in add-in errors.

use rmcp::ErrorData as McpError;

use crate::addin::proto;

/// Most names listed in one sentence of a reply.
pub const LISTED: usize = 30;

/// Appends "(entity 1 = line:3, entity 2 = point:5)" to an add-in error that
/// talks about request items by number, since the model knows them by id.
pub fn with_legend(error: McpError, noun: &str, given: &[String]) -> McpError {
    let marker = format!("{noun} ");
    let numbered = error.message.match_indices(&marker).any(|(at, _)| {
        error.message[at + marker.len()..].starts_with(|c: char| c.is_ascii_digit())
    });
    if !numbered || given.len() < 2 {
        return error;
    }
    let legend: Vec<String> = given
        .iter()
        .take(LISTED)
        .enumerate()
        .map(|(i, g)| format!("{noun} {} = {}", i + 1, g.trim()))
        .collect();
    let more = if given.len() > LISTED { ", ..." } else { "" };
    McpError::new(
        error.code,
        format!("{} ({}{more})", error.message, legend.join(", ")),
        error.data,
    )
}

/// A readable kind for SolidWorks feature type names (IFeature::GetTypeName2).
pub fn type_label(type_name: &str) -> &str {
    match type_name {
        "ProfileFeature" => "sketch",
        "3DProfileFeature" => "3D sketch",
        "Helix" => "helix",
        "Extrusion" | "BaseBody" | "Boss" => "boss-extrude",
        "Cut" => "cut-extrude",
        // SolidWorks 2026 types both a boss on an existing body and a cut "ICE".
        "ICE" => "extrude",
        "Revolution" | "RevCut" => "revolve",
        "Sweep" | "SweepCut" => "sweep",
        "Blend" | "BlendCut" => "loft",
        "HoleWzd" | "AdvHoleWzd" => "hole",
        "Fillet" | "VarFillet" | "FaceFillet" | "FullRoundFillet" => "fillet",
        "Chamfer" => "chamfer",
        "Shell" => "shell",
        "Draft" => "draft",
        "Rib" => "rib",
        "LPattern" => "linear pattern",
        "CirPattern" => "circular pattern",
        "MirrorPattern" | "MirrorSolid" => "mirror",
        "RefPlane" => "plane",
        "RefAxis" => "axis",
        "RefPoint" => "point",
        "CoordSys" => "coordinate system",
        "FtrFolder" => "folder",
        t if t.starts_with("Mate") => "mate",
        t => t,
    }
}

/// What a swFeatureError_e code means, roughly (values from swconst.tlh).
pub fn feature_error_name(code: i32) -> String {
    match code {
        1 => "unknown error (often a missing reference)".into(),
        10..=19 => "the fillet can't be made (its edges/faces are gone or changed)".into(),
        30 => "the extrusion would make disjoint bodies".into(),
        31 => "the extrusion's end condition can't be reached".into(),
        32 => "bad geometric conditions for the extrusion".into(),
        33..=37 => "the sketch profile isn't valid any more (open, or open and closed contours)".into(),
        38 | 39 | 41 | 44 => "the mated entities don't suit this mate type".into(),
        40 => "SolidWorks couldn't make a surface for a mated entity".into(),
        42 => "unknown tangency".into(),
        43 | 48 => "a mated entity is missing".into(),
        45 => "a mated entity failed".into(),
        46 => "over-defined: conflicts with other mates or fixed components".into(),
        47 => "can't be solved (ill-defined)".into(),
        49 => "the feature type is deprecated".into(),
        50 => "the feature type is obsolete".into(),
        51 => "an external sketch reference failed (the geometry it referenced is gone)".into(),
        52..=68 => "the partial edge fillet can't be made (its edge or reference entities are gone or changed)".into(),
        69 => "the swept flange's profile or path isn't valid".into(),
        70 => "the swept flange intersects itself".into(),
        71 => "the cut doesn't intersect the model".into(),
        72 => "the sketch has a self-intersecting contour".into(),
        73 => "items the feature uses are missing".into(),
        code => format!("SolidWorks error {code}"),
    }
}

/// The features a change made fail to rebuild, or rebuild with warnings.
pub fn problems_text(problems: &[proto::FeatureProblem]) -> String {
    let describe = |p: &proto::FeatureProblem| {
        let label = type_label(&p.type_name);
        let what = if label.is_empty() {
            p.name.clone()
        } else {
            format!("{} ({label})", p.name)
        };
        format!("{what}: {}", feature_error_name(p.error_code))
    };
    let (warnings, errors): (Vec<_>, Vec<_>) = problems.iter().partition(|p| p.warning);
    let mut text = String::new();
    if !errors.is_empty() {
        let list: Vec<String> = errors.iter().take(LISTED).map(|p| describe(p)).collect();
        text += &format!(" Now failing to rebuild: {}.", list.join("; "));
    }
    if !warnings.is_empty() {
        let list: Vec<String> = warnings.iter().take(LISTED).map(|p| describe(p)).collect();
        text += &format!(" Now rebuilding with warnings: {}.", list.join("; "));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legend_only_for_numbered_messages() {
        let given = vec!["line:3".to_string(), "point:5".to_string()];
        let e = with_legend(
            McpError::internal_error("entity 2 is an end point of Line1", None),
            "entity",
            &given,
        );
        assert!(
            e.message
                .ends_with("(entity 1 = line:3, entity 2 = point:5)"),
            "{}",
            e.message
        );
        let e = with_legend(
            McpError::internal_error("the entity sketch is gone", None),
            "entity",
            &given,
        );
        assert_eq!(e.message, "the entity sketch is gone");
    }
}

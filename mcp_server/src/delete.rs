//! Deleting: the delete_features and delete_sketch_entities tools' params,
//! and the text they reply with.
//!
//! Features are named by inspect_model ids ("feature:4", which includes an
//! assembly's mates) or by their feature-tree names; sketch entities by the
//! sketch ids the sketch tools hand out ("line:3"). The add-in checks
//! everything before it deletes anything, and refuses by default to delete
//! a feature other features depend on, so the model has to say what should
//! happen to them.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::inspect::{DRAWING_KINDS, IdRegistry, Kind, SKETCH_KINDS};
use crate::sketch;

/// Most features or sketch entities per call (the add-in's limits).
pub const MAX_FEATURES: usize = 500;
pub const MAX_SKETCH_ENTITIES: usize = 1000;
/// Most names listed in one sentence of a reply.
const LISTED: usize = 30;

/// What happens to features that depend on the deleted ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Children {
    /// Don't delete anything if other features depend on these; the error names them
    /// (default).
    #[default]
    Refuse,
    /// Delete the dependent features too.
    Delete,
    /// Keep them; they lose their references to the deleted features and may fail to
    /// rebuild.
    Keep,
}

impl From<Children> for proto::ChildFeatures {
    fn from(children: Children) -> Self {
        match children {
            Children::Refuse => proto::ChildFeatures::Refuse,
            Children::Delete => proto::ChildFeatures::Delete,
            Children::Keep => proto::ChildFeatures::Keep,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteFeaturesParams {
    /// The features to delete: "feature:N" ids from inspect_model (the features list, and in
    /// an assembly its mates) or feature-tree names such as "Cut-Extrude1", "Sketch3",
    /// "Fillet1", "Plane1" or "Coincident2".
    pub features: Vec<String>,
    /// Features that depend on these (a fillet on an extrude's edges, a sketch on its face,
    /// the extrude made from a sketch): "refuse" (default) deletes nothing and names them,
    /// "delete" deletes them too, "keep" leaves them (they lose those references and may fail
    /// to rebuild).
    #[serde(default)]
    pub children: Children,
    /// Also delete what the features absorbed, e.g. an extrude's or cut's profile sketch.
    /// Default false: like SolidWorks, the sketch stays in the tree. (A Hole Wizard hole's own
    /// sketches always go with it; a sketch given to hole_wizard as sketch_name stays.)
    #[serde(default)]
    pub delete_absorbed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteSketchEntitiesParams {
    /// Sketch entity ids from inspect_sketch or the sketch_* tools, all in one sketch:
    /// segments ("line:3", "arc:1", "circle:2", "spline:1", ...) and standalone points
    /// ("point:7", e.g. hole centers). A segment's end and center points go with it; on
    /// their own they can't be deleted.
    pub entities: Vec<String>,
    /// The sketch, e.g. "Sketch1". Defaults to the sketch the ids belong to.
    #[serde(default)]
    pub sketch_name: Option<String>,
}

fn invalid(message: String) -> McpError {
    McpError::invalid_params(message, None)
}

/// The wire targets for delete_features, checked: ids must be feature ids
/// this server handed out; anything else is taken as a feature name.
pub fn feature_targets(
    ids: &IdRegistry,
    features: &[String],
) -> Result<Vec<proto::FeatureTarget>, McpError> {
    use proto::feature_target::Target;
    if features.is_empty() || features.len() > MAX_FEATURES {
        return Err(invalid(format!("give between 1 and {MAX_FEATURES} features")));
    }
    features
        .iter()
        .enumerate()
        .map(|(i, feature)| {
            let feature = feature.trim();
            let what = format!("feature {}", i + 1);
            if feature.is_empty() {
                return Err(invalid(format!("{what} is empty")));
            }
            let target = match Kind::of_id(feature) {
                None => Target::Name(feature.to_string()),
                Some(Kind::Feature) => Target::PersistId(ids.persist_id(feature).ok_or_else(|| {
                    let why = ids.unknown_id(
                        feature,
                        "inspect_model",
                        "call inspect_model for current ones, or give the feature's name",
                    );
                    invalid(format!("{what}: {why}"))
                })?),
                Some(kind) if SKETCH_KINDS.contains(&kind) => {
                    return Err(invalid(format!(
                        "{what}: {feature} is a sketch entity -- use delete_sketch_entities for \
                         those (or give the sketch's name to delete the whole sketch)"
                    )));
                }
                Some(kind) if DRAWING_KINDS.contains(&kind) => {
                    return Err(invalid(format!(
                        "{what}: {feature} is a drawing view entity, not a feature"
                    )));
                }
                Some(Kind::Component) => {
                    return Err(invalid(format!(
                        "{what}: {feature} is a component -- deleting components isn't supported \
                         yet"
                    )));
                }
                Some(_) => {
                    return Err(invalid(format!(
                        "{what}: {feature} is a face, edge or body, not a feature -- give the \
                         feature that made it (inspect_model what=\"features\"), by id or name"
                    )));
                }
            };
            Ok(proto::FeatureTarget { target: Some(target) })
        })
        .collect()
}

/// The wire refs for delete_sketch_entities.
pub fn sketch_items(
    ids: &IdRegistry,
    entities: &[String],
) -> Result<Vec<proto::SketchItemRef>, McpError> {
    if entities.is_empty() || entities.len() > MAX_SKETCH_ENTITIES {
        return Err(invalid(format!(
            "give between 1 and {MAX_SKETCH_ENTITIES} sketch entities"
        )));
    }
    entities
        .iter()
        .enumerate()
        .map(|(i, entity)| {
            let entity = entity.trim();
            let what = format!("entity {}", i + 1);
            match Kind::of_id(entity) {
                Some(kind) if SKETCH_KINDS.contains(&kind) => {
                    sketch::decode_ref(ids, entity).ok_or_else(|| {
                        let why = ids.unknown_id(
                            entity,
                            "inspect_sketch and the sketch_* tools",
                            "call inspect_sketch for current ones",
                        );
                        invalid(format!("{what}: {why}"))
                    })
                }
                Some(Kind::Feature) => Err(invalid(format!(
                    "{what}: {entity} is a feature -- use delete_features for features, \
                     including whole sketches"
                ))),
                Some(_) => Err(invalid(format!(
                    "{what}: {entity} isn't a sketch entity -- give ids like \"line:3\" or \
                     \"point:7\" from inspect_sketch"
                ))),
                None if entity.eq_ignore_ascii_case("origin") => Err(invalid(format!(
                    "{what}: the sketch origin can't be deleted"
                ))),
                None => Err(invalid(format!(
                    "{what}: {entity:?} isn't a sketch entity id -- give ids like \"line:3\" or \
                     \"point:7\" from inspect_sketch"
                ))),
            }
        })
        .collect()
}

/// Appends "(entity 1 = line:3, entity 2 = point:5)" to an add-in error that
/// talks about request items by number, since the model knows them by id.
pub fn with_legend(error: McpError, noun: &str, given: &[String]) -> McpError {
    let marker = format!("{noun} ");
    let numbered = error
        .message
        .match_indices(&marker)
        .any(|(at, _)| error.message[at + marker.len()..].starts_with(|c: char| c.is_ascii_digit()));
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
        "Extrusion" | "BaseBody" | "Boss" => "boss-extrude",
        "Cut" => "cut-extrude",
        // SolidWorks 2026 types both a boss on an existing body and a cut "ICE".
        "ICE" => "extrude",
        "Revolution" | "RevCut" => "revolve",
        "HoleWzd" | "AdvHoleWzd" => "hole",
        "Fillet" | "VarFillet" | "FaceFillet" | "FullRoundFillet" => "fillet",
        "Chamfer" => "chamfer",
        "Shell" => "shell",
        "Draft" => "draft",
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

fn feature_list(features: &[proto::FeatureRef]) -> String {
    let mut parts: Vec<String> = features
        .iter()
        .take(LISTED)
        .map(|f| {
            let label = type_label(&f.type_name);
            if label.is_empty() {
                f.name.clone()
            } else {
                format!("{} ({label})", f.name)
            }
        })
        .collect();
    if features.len() > LISTED {
        parts.push(format!("and {} more", features.len() - LISTED));
    }
    parts.join(", ")
}

fn plural(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
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

pub fn features_reply_text(params: &DeleteFeaturesParams, reply: &proto::DeleteFeaturesResponse) -> String {
    let mut text = format!(
        "deleted {}: {}.",
        plural(reply.deleted.len(), "feature", "features"),
        feature_list(&reply.deleted)
    );
    if !reply.not_deleted.is_empty() {
        text += &format!(
            " SolidWorks didn't delete {}.",
            feature_list(&reply.not_deleted)
        );
    }
    let (they, them) = if reply.deleted.len() == 1 { ("it", "it") } else { ("they", "them") };
    if !reply.kept_absorbed.is_empty() {
        text += &format!(
            " Kept what {they} had absorbed, now standalone in the tree: {} (delete_absorbed=true \
             deletes that too).",
            feature_list(&reply.kept_absorbed)
        );
    }
    if !reply.kept_children.is_empty() {
        let how = if params.children == Children::Keep {
            "as asked"
        } else {
            "SolidWorks didn't delete them"
        };
        text += &format!(
            " Kept the features that depended on {them} ({how}): {}.",
            feature_list(&reply.kept_children)
        );
    }
    text += &problems_text(&reply.new_errors);
    text += " The model was rebuilt; ids of deleted features (and their faces/edges) no longer \
             work -- inspect_model again for current ones.";
    text
}

pub fn sketch_reply_text(
    ids: &mut IdRegistry,
    given: &[String],
    reply: &proto::DeleteSketchEntitiesResponse,
) -> String {
    let kept: Vec<String> = reply
        .kept
        .iter()
        .filter_map(|r| {
            let kind = if r.point { Kind::SketchPoint } else { Kind::Segment };
            // Prefer the id the model used for it.
            given
                .iter()
                .map(|g| g.trim())
                .find(|g| sketch::decode_ref(ids, g).as_ref() == Some(r))
                .map(str::to_string)
                .or_else(|| sketch::ref_id(ids, kind, r))
        })
        .collect();
    let deleted: Vec<&str> = given
        .iter()
        .map(|g| g.trim())
        .filter(|g| !kept.iter().any(|k| k == g))
        .take(LISTED)
        .collect();

    let mut text = format!(
        "deleted {} from {}",
        plural(reply.deleted.max(0) as usize, "entity", "entities"),
        reply.sketch_name
    );
    if !deleted.is_empty() {
        text += &format!(" ({})", deleted.join(", "));
    }
    let mut also = Vec::new();
    if reply.points_removed > 0 {
        also.push(plural(reply.points_removed as usize, "end/center point", "end/center points"));
    }
    if !reply.dimensions_removed.is_empty() {
        also.push(format!(
            "{} ({})",
            plural(reply.dimensions_removed.len(), "dimension", "dimensions"),
            reply.dimensions_removed.join(", ")
        ));
    }
    if reply.relations_removed > 0 {
        also.push(plural(reply.relations_removed as usize, "relation", "relations"));
    }
    if !also.is_empty() {
        text += &format!(", along with {}", also.join(", "));
    }
    text.push('.');
    if !kept.is_empty() {
        text += &format!(" SolidWorks didn't delete {}.", kept.join(", "));
    }
    if let Some(c) = &reply.contours {
        text += &format!(
            " {} now has {} and {}.",
            reply.sketch_name,
            plural(c.closed.max(0) as usize, "closed contour", "closed contours"),
            plural(c.open.max(0) as usize, "open contour", "open contours")
        );
    }
    text += &sketch::state_text(ids, &reply.sketch_name, reply.state.as_ref());
    text += &problems_text(&reply.new_errors);
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    fn registry() -> (IdRegistry, String, String) {
        let mut ids = IdRegistry::default();
        let feature = ids.id_for(Kind::Feature, b"extrude-bytes").unwrap();
        let line = sketch::ref_id(
            &mut ids,
            Kind::Line,
            &proto::SketchItemRef { sketch_persist_id: b"sk".to_vec(), point: false, id0: 3, id1: 4 },
        )
        .unwrap();
        (ids, feature, line)
    }

    #[test]
    fn feature_targets_take_ids_and_names() {
        use proto::feature_target::Target;
        let (ids, feature, _) = registry();
        let targets =
            feature_targets(&ids, &[feature.clone(), " Cut-Extrude1 ".to_string()]).unwrap();
        assert_eq!(targets[0].target, Some(Target::PersistId(b"extrude-bytes".to_vec())));
        assert_eq!(targets[1].target, Some(Target::Name("Cut-Extrude1".into())));
    }

    #[test]
    fn feature_targets_reject_other_ids() {
        let (ids, _, line) = registry();
        for bad in [line.as_str(), "face:1", "component:2", "feature:99", "", "view_edge:1"] {
            assert!(feature_targets(&ids, &[bad.to_string()]).is_err(), "{bad:?} accepted");
        }
        assert!(feature_targets(&ids, &[]).is_err());
    }

    #[test]
    fn sketch_items_decode_ids() {
        let (ids, feature, line) = registry();
        let items = sketch_items(&ids, &[line]).unwrap();
        assert_eq!((items[0].id0, items[0].id1, items[0].point), (3, 4, false));
        for bad in [feature.as_str(), "origin", "Line3", "point:12", "face:1"] {
            assert!(sketch_items(&ids, &[bad.to_string()]).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn legend_only_for_numbered_messages() {
        let given = vec!["line:3".to_string(), "point:5".to_string()];
        let e = with_legend(McpError::internal_error("entity 2 is an end point of Line1", None), "entity", &given);
        assert!(e.message.ends_with("(entity 1 = line:3, entity 2 = point:5)"), "{}", e.message);
        let e = with_legend(McpError::internal_error("the entity sketch is gone", None), "entity", &given);
        assert_eq!(e.message, "the entity sketch is gone");
    }

    #[test]
    fn features_text_mentions_what_stayed_and_broke() {
        let params = DeleteFeaturesParams {
            features: vec!["Boss-Extrude1".into()],
            children: Children::Keep,
            delete_absorbed: false,
        };
        let f = |name: &str, type_name: &str| proto::FeatureRef { name: name.into(), type_name: type_name.into() };
        let reply = proto::DeleteFeaturesResponse {
            deleted: vec![f("Boss-Extrude1", "Extrusion")],
            not_deleted: vec![],
            kept_absorbed: vec![f("Sketch1", "ProfileFeature")],
            kept_children: vec![f("Fillet1", "Fillet")],
            new_errors: vec![proto::FeatureProblem {
                name: "Fillet1".into(),
                type_name: "Fillet".into(),
                error_code: 13,
                warning: false,
            }],
        };
        let text = features_reply_text(&params, &reply);
        assert!(text.starts_with("deleted 1 feature: Boss-Extrude1 (boss-extrude)."), "{text}");
        assert!(text.contains("Sketch1 (sketch)"), "{text}");
        assert!(text.contains("(as asked): Fillet1 (fillet)"), "{text}");
        assert!(text.contains("Now failing to rebuild: Fillet1 (fillet): the fillet"), "{text}");
    }

    #[test]
    fn sketch_text_lists_what_went() {
        let (mut ids, _, line) = registry();
        let kept_ref = proto::SketchItemRef { sketch_persist_id: b"sk".to_vec(), point: true, id0: 9, id1: 9 };
        let point = sketch::ref_id(&mut ids, Kind::SketchPoint, &kept_ref).unwrap();
        let reply = proto::DeleteSketchEntitiesResponse {
            sketch_name: "Sketch1".into(),
            deleted: 1,
            kept: vec![kept_ref.clone()],
            points_removed: 2,
            dimensions_removed: vec!["D1@Sketch1".into()],
            relations_removed: 1,
            state: None,
            contours: Some(proto::SketchContours { closed: 0, open: 1 }),
            new_errors: vec![],
        };
        let text = sketch_reply_text(&mut ids, &[line.clone(), point.clone()], &reply);
        assert!(text.starts_with(&format!("deleted 1 entity from Sketch1 ({line}), along with 2 end/center points, 1 dimension (D1@Sketch1), 1 relation.")), "{text}");
        assert!(text.contains(&format!("didn't delete {point}")), "{text}");
        assert!(text.contains("0 closed contours and 1 open contour"), "{text}");
        // The kept ref round-trips through the registry unchanged.
        assert_eq!(kept_ref.encode_to_vec(), ids.persist_id(&point).unwrap());
    }
}

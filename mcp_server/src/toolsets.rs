//! Tool groups: which SolidWorks mode(s) a tool works in, and whether it
//! inspects, edits or can destroy work. The server can be started on a subset
//! (`--modes`, `--inspect-only`) so a client without tool search lists fewer
//! tools; access also gives each tool's MCP annotations.

use rmcp::model::ToolAnnotations;
use Access::{Destructive, Edit, Inspect};
use Mode::{Assembly, Drawing, Modeling};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Modeling,
    Assembly,
    Drawing,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    /// Reads the model or makes a picture of it; changes nothing.
    Inspect,
    /// Creates things or changes settings, in a way undo or another call
    /// takes back.
    Edit,
    /// Can delete, discard unsaved changes, overwrite a file, or (run_macro)
    /// do anything; at least one of its actions does.
    Destructive,
}

const ALL: &[Mode] = &[Modeling, Assembly, Drawing];
const MODEL: &[Mode] = &[Modeling, Assembly];
const PART: &[Mode] = &[Modeling];
const ASM: &[Mode] = &[Assembly];
const DRW: &[Mode] = &[Drawing];

/// Every tool: name, the modes it works in, access.
pub const TOOLS: &[(&str, &[Mode], Access)] = &[
    // Any mode: documents, session, properties, annotations a part, assembly
    // or drawing can all carry.
    ("get_active_document_name", ALL, Inspect),
    ("create_part", ALL, Edit),
    ("create_assembly", ALL, Edit),
    ("create_drawing", ALL, Edit),
    ("open_document", ALL, Edit),
    ("save_document", ALL, Destructive),
    ("close_document", ALL, Destructive),
    ("file_references", ALL, Inspect),
    ("document_units", ALL, Edit),
    ("custom_properties", ALL, Edit),
    ("undo", ALL, Destructive),
    ("run_macro", ALL, Destructive),
    ("add_geometric_tolerance", ALL, Edit),
    ("add_datum_symbol", ALL, Edit),
    ("add_surface_finish_symbol", ALL, Edit),
    ("add_weld_symbol", ALL, Edit),
    ("set_dimension_tolerance", ALL, Edit),
    // Part or assembly: the feature tree, configurations, the model's geometry.
    ("inspect_model", MODEL, Inspect),
    ("inspect_annotations", MODEL, Inspect),
    ("diagnose_model", MODEL, Inspect),
    ("measure", MODEL, Inspect),
    ("mass_properties", MODEL, Inspect),
    ("render_views", MODEL, Inspect),
    ("set_dimension", MODEL, Edit),
    ("set_visibility", MODEL, Edit),
    ("set_suppression", MODEL, Edit),
    ("rollback", MODEL, Edit),
    ("reorder_feature", MODEL, Edit),
    ("delete_features", MODEL, Destructive),
    ("configurations", MODEL, Destructive),
    ("equations", MODEL, Destructive),
    ("design_table", MODEL, Destructive),
    // Part modeling: sketches and features.
    ("inspect_sketch", PART, Inspect),
    ("inspect_feature", PART, Inspect),
    ("recognize_features", PART, Inspect),
    ("sketch", PART, Edit),
    ("sketch_fillet", PART, Edit),
    ("sketch_chamfer", PART, Edit),
    ("trim_sketch_entity", PART, Destructive),
    ("extend_sketch_entity", PART, Edit),
    ("convert_entities", PART, Edit),
    ("offset_sketch_entities", PART, Edit),
    ("mirror_sketch_entities", PART, Edit),
    ("linear_pattern_sketch_entities", PART, Edit),
    ("circular_pattern_sketch_entities", PART, Edit),
    ("delete_sketch_entities", PART, Destructive),
    ("dimension_sketch", PART, Edit),
    ("add_sketch_relations", PART, Edit),
    ("fully_define_sketch", PART, Edit),
    ("extrude_sketch", PART, Edit),
    ("revolve_sketch", PART, Edit),
    ("sweep_sketch", PART, Edit),
    ("loft_sketch", PART, Edit),
    ("hole_wizard", PART, Edit),
    ("fillet_edges", PART, Edit),
    ("chamfer_edges", PART, Edit),
    ("shell_body", PART, Edit),
    ("rib_sketch", PART, Edit),
    ("draft_faces", PART, Edit),
    ("create_helix", PART, Edit),
    ("mirror_features", PART, Edit),
    ("linear_pattern_features", PART, Edit),
    ("circular_pattern_features", PART, Edit),
    ("create_reference_geometry", PART, Edit),
    ("edit_extrude", PART, Edit),
    ("edit_fillet", PART, Edit),
    ("edit_pattern", PART, Edit),
    ("edit_sweep", PART, Edit),
    ("part_material", PART, Destructive),
    // Assembly: components and mates. check_interference also runs from a
    // drawing of the assembly.
    ("inspect_mobility", ASM, Inspect),
    ("check_interference", &[Assembly, Drawing], Inspect),
    ("insert_components", ASM, Edit),
    ("replace_components", ASM, Destructive),
    ("fix_components", ASM, Edit),
    ("move_component", ASM, Edit),
    ("add_mate", ASM, Edit),
    ("edit_mate", ASM, Edit),
    // Leaves the mate moved with restore=false.
    ("sweep_mate", ASM, Edit),
    // Drawing: sheets, views, annotations, tables, title block.
    ("inspect_drawing", DRW, Inspect),
    ("inspect_drawing_annotations", DRW, Inspect),
    ("inspect_drawing_tables", DRW, Inspect),
    ("inspect_title_block", DRW, Inspect),
    ("render_drawing", DRW, Inspect),
    ("add_sheet", DRW, Edit),
    ("activate_sheet", DRW, Edit),
    ("edit_sheet", DRW, Edit),
    ("draw_sheet_border", DRW, Edit),
    ("draw_title_block", DRW, Edit),
    ("fill_title_block", DRW, Edit),
    ("drawing_properties", DRW, Edit),
    ("create_drawing_view", DRW, Edit),
    ("edit_drawing_view", DRW, Edit),
    ("crop_view", DRW, Edit),
    ("break_view", DRW, Edit),
    ("add_drawing_dimensions", DRW, Edit),
    ("insert_model_items", DRW, Edit),
    ("add_center_marks", DRW, Edit),
    ("add_centerlines", DRW, Edit),
    ("add_drawing_note", DRW, Edit),
    ("edit_drawing_annotation", DRW, Edit),
    ("delete_drawing_annotations", DRW, Destructive),
    ("add_drawing_table", DRW, Edit),
    ("edit_drawing_table", DRW, Edit),
    ("add_balloons", DRW, Edit),
];

/// Which tools to list: the core (tools of all three modes) and those in any
/// of `modes`, only inspecting ones if `inspect_only`.
#[derive(Debug, PartialEq)]
pub struct Filter {
    pub modes: Vec<Mode>,
    pub inspect_only: bool,
}

impl Default for Filter {
    fn default() -> Self {
        Self { modes: ALL.to_vec(), inspect_only: false }
    }
}

impl Filter {
    /// From the command line: `--modes modeling,assembly,drawing`,
    /// `--inspect-only`. No arguments: every tool.
    pub fn from_args(args: impl IntoIterator<Item = String>) -> anyhow::Result<Self> {
        let mut filter = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--inspect-only" => filter.inspect_only = true,
                "--modes" => {
                    let list = args.next().ok_or_else(|| anyhow::anyhow!("--modes needs a list"))?;
                    filter.modes = list
                        .split(',')
                        .map(|m| match m.trim() {
                            "modeling" => Ok(Modeling),
                            "assembly" => Ok(Assembly),
                            "drawing" => Ok(Drawing),
                            other => anyhow::bail!(
                                "unknown mode {other:?}; modes are modeling, assembly, drawing"
                            ),
                        })
                        .collect::<anyhow::Result<_>>()?;
                }
                other => anyhow::bail!("unknown argument {other:?}; use --modes, --inspect-only"),
            }
        }
        Ok(filter)
    }

    /// Whether to list `tool`. Tools missing from TOOLS are listed (the test
    /// keeps TOOLS complete).
    pub fn allows(&self, tool: &str) -> bool {
        match TOOLS.iter().find(|(name, ..)| *name == tool) {
            Some((_, modes, access)) => {
                let core = modes.len() == ALL.len();
                (core || modes.iter().any(|m| self.modes.contains(m)))
                    && !(self.inspect_only && *access != Inspect)
            }
            None => true,
        }
    }
}

/// `tool`'s annotations: read-only, or destructive or not. None for a tool
/// missing from TOOLS.
pub fn annotations(tool: &str) -> Option<ToolAnnotations> {
    let (_, _, access) = TOOLS.iter().find(|(name, ..)| *name == tool)?;
    Some(match access {
        Inspect => ToolAnnotations::new().read_only(true),
        Edit => ToolAnnotations::new().read_only(false).destructive(false),
        Destructive => ToolAnnotations::new().read_only(false).destructive(true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> impl Iterator<Item = String> + '_ {
        s.split_whitespace().map(String::from)
    }

    #[test]
    fn parses_arguments() {
        assert_eq!(Filter::from_args(args("")).unwrap(), Filter::default());
        let f = Filter::from_args(args("--modes drawing,assembly --inspect-only")).unwrap();
        assert_eq!(f, Filter { modes: vec![Drawing, Assembly], inspect_only: true });
        assert!(Filter::from_args(args("--dynamic")).is_err());
        assert!(Filter::from_args(args("--modes part")).is_err());
        assert!(Filter::from_args(args("--modes")).is_err());
        assert!(Filter::from_args(args("--verbose")).is_err());
    }

    #[test]
    fn filters_by_mode_and_access() {
        let drawing = Filter::from_args(args("--modes drawing")).unwrap();
        assert!(drawing.allows("create_drawing_view"));
        assert!(drawing.allows("save_document"));
        assert!(drawing.allows("check_interference"));
        assert!(!drawing.allows("extrude_sketch"));
        assert!(!drawing.allows("add_mate"));
        let inspect = Filter::from_args(args("--modes modeling --inspect-only")).unwrap();
        assert!(inspect.allows("inspect_model"));
        assert!(!inspect.allows("sketch"));
        assert!(!inspect.allows("delete_features"));
        assert!(!inspect.allows("inspect_drawing"));
    }

    #[test]
    fn annotations_follow_access() {
        let hints = |tool| {
            let a = annotations(tool).unwrap();
            (a.read_only_hint, a.destructive_hint)
        };
        assert_eq!(hints("measure"), (Some(true), None));
        assert_eq!(hints("fillet_edges"), (Some(false), Some(false)));
        assert_eq!(hints("delete_features"), (Some(false), Some(true)));
        assert!(annotations("no_such_tool").is_none());
    }
}

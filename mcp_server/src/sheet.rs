//! The drawing sheet tools: add_sheet, activate_sheet and edit_sheet, all
//! over the SetupSheet RPC. Params, the scale's "1:2" form, and the reply
//! text, which flags views that a new size or scale pushed past the sheet's
//! edge and a title block kept on paper of another size.

use rmcp::{ErrorData as McpError, schemars};
use serde::Deserialize;

use crate::addin::proto;
use crate::drawing::scale_text;
use crate::files::{self, PaperSize};
use crate::inspect::MM_PER_M;

/// swDrawingViewTypes_e's projected view.
const PROJECTED_VIEW: i32 = 4;

/// First- or third-angle projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Projection {
    /// ISO / European: the view of the part's top goes below the front view, its left side
    /// to the right of it.
    FirstAngle,
    /// ANSI / American: the top view goes above the front view, the right side to its right.
    ThirdAngle,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct AddSheetParams {
    /// The new sheet's name. Default: the next free "SheetN".
    #[serde(default)]
    pub name: Option<String>,
    /// Standard paper size. Default: the active sheet's, or the sheet format's own size when
    /// sheet_format is given.
    #[serde(default)]
    pub paper_size: Option<PaperSize>,
    /// A custom sheet size instead of paper_size: [width, height] in mm.
    #[serde(default)]
    pub paper_size_mm: Option<[f64; 2]>,
    /// Sheet format, the border and title block (.slddrt): an absolute path, or the name of one
    /// in SolidWorks' sheet format folder (e.g. "a4 - iso"); an unknown name lists the ones
    /// there are. Default: the active sheet's.
    #[serde(default)]
    pub sheet_format: Option<String>,
    /// Sheet scale as "numerator:denominator", e.g. "1:2" (half size) or "2:1". Views that use
    /// the sheet scale follow it. Default: the active sheet's.
    #[serde(default)]
    pub scale: Option<String>,
    /// Default: the active sheet's.
    #[serde(default)]
    pub projection: Option<Projection>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ActivateSheetParams {
    /// The sheet's name, e.g. "Sheet2" (inspect_drawing lists them).
    pub sheet: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EditSheetParams {
    /// The sheet to change, e.g. "Sheet2". Default: the active sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Standard paper size.
    #[serde(default)]
    pub paper_size: Option<PaperSize>,
    /// A custom sheet size instead of paper_size: [width, height] in mm.
    #[serde(default)]
    pub paper_size_mm: Option<[f64; 2]>,
    /// Another sheet format, the border and title block (.slddrt): an absolute path, or the
    /// name of one in SolidWorks' sheet format folder (e.g. "a4 - iso"). Without a paper size
    /// the sheet takes the format's own size.
    #[serde(default)]
    pub sheet_format: Option<String>,
    /// Sheet scale as "numerator:denominator", e.g. "1:2" (half size) or "2:1". Views that use
    /// the sheet scale follow it, and views projected from them.
    #[serde(default)]
    pub scale: Option<String>,
    /// Projected views stay where they are and show what this projection puts there (in
    /// first-angle the view above the front view shows the part's bottom, in third-angle its
    /// top).
    #[serde(default)]
    pub projection: Option<Projection>,
    /// A new name for the sheet.
    #[serde(default)]
    pub rename: Option<String>,
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

/// "1:2" -> (1, 2).
fn parse_scale(scale: &str) -> Result<(f64, f64), McpError> {
    let bad = || {
        invalid(format!(
            "scale {scale:?}: give it as \"numerator:denominator\", both above 0, e.g. \"1:2\" \
             (half size) or \"2:1\" (twice the size)"
        ))
    };
    let (n, d) = scale.split_once(':').ok_or_else(bad)?;
    let n: f64 = n.trim().parse().map_err(|_| bad())?;
    let d: f64 = d.trim().parse().map_err(|_| bad())?;
    if !(n.is_finite() && d.is_finite() && n > 0.0 && d > 0.0) {
        return Err(bad());
    }
    Ok((n, d))
}

/// A sheet name without surrounding blanks; None for a missing or blank one.
fn name(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
}

/// The setup shared by add_sheet and edit_sheet, into `request`.
fn set_setup(
    request: &mut proto::SetupSheetRequest,
    paper_size: Option<PaperSize>,
    paper_size_mm: Option<[f64; 2]>,
    sheet_format: Option<&str>,
    scale: Option<&str>,
    projection: Option<Projection>,
) -> Result<(), McpError> {
    (request.paper_size, request.width, request.height) = files::paper(paper_size, paper_size_mm)?;
    request.sheet_format = files::template_file(sheet_format, "slddrt", "sheet format")?;
    if let Some(scale) = scale {
        let (n, d) = parse_scale(scale)?;
        request.scale_numerator = Some(n);
        request.scale_denominator = d;
    }
    request.first_angle = projection.map(|p| p == Projection::FirstAngle);
    Ok(())
}

pub fn add_request(params: &AddSheetParams) -> Result<proto::SetupSheetRequest, McpError> {
    let mut request = proto::SetupSheetRequest {
        sheet_name: name(params.name.as_deref()).unwrap_or_default(),
        add: true,
        ..Default::default()
    };
    set_setup(
        &mut request,
        params.paper_size,
        params.paper_size_mm,
        params.sheet_format.as_deref(),
        params.scale.as_deref(),
        params.projection,
    )?;
    Ok(request)
}

pub fn activate_request(params: &ActivateSheetParams) -> Result<proto::SetupSheetRequest, McpError> {
    let sheet = name(Some(&params.sheet)).ok_or_else(|| invalid("give the sheet's name, e.g. \"Sheet2\""))?;
    Ok(proto::SetupSheetRequest { sheet_name: sheet, activate: true, ..Default::default() })
}

pub fn edit_request(params: &EditSheetParams) -> Result<proto::SetupSheetRequest, McpError> {
    if params.rename.as_ref().is_some_and(|r| r.trim().is_empty()) {
        return Err(invalid("rename: give the sheet's new name"));
    }
    let mut request = proto::SetupSheetRequest {
        sheet_name: name(params.sheet.as_deref()).unwrap_or_default(),
        new_name: name(params.rename.as_deref()).unwrap_or_default(),
        ..Default::default()
    };
    set_setup(
        &mut request,
        params.paper_size,
        params.paper_size_mm,
        params.sheet_format.as_deref(),
        params.scale.as_deref(),
        params.projection,
    )?;
    if request.paper_size.is_none()
        && request.sheet_format.is_empty()
        && request.scale_numerator.is_none()
        && request.first_angle.is_none()
        && request.new_name.is_empty()
    {
        return Err(invalid(
            "nothing to change: give paper_size / paper_size_mm, sheet_format, scale, projection \
             or rename (activate_sheet shows a sheet)",
        ));
    }
    Ok(request)
}

/// "Drawing View1 at 1:2 (sheet scale)" for each view; the views reaching
/// past the sheet's edges.
fn views_text(sheet: &proto::SheetInfo) -> (String, Vec<String>) {
    let mut views = Vec::new();
    let mut outside = Vec::new();
    // Outlines are sheet meters; 0.1 mm slack for rounding.
    let slack = 1e-4;
    for v in &sheet.views {
        let mut text = v.name.clone();
        if let Some(scale) = scale_text(v.scale_numerator, v.scale_denominator) {
            text += &format!(" at {scale}");
        }
        if v.uses_sheet_scale {
            text += " (sheet scale)";
        } else if v.r#type == PROJECTED_VIEW && !v.base_view.is_empty() {
            text += &format!(" (projected from {})", v.base_view);
        }
        views.push(text);
        if let Some(outline) = &v.outline {
            let (lo, hi) = (outline.min.clone().unwrap_or_default(), outline.max.clone().unwrap_or_default());
            if lo.x < -slack || lo.y < -slack || hi.x > sheet.width + slack || hi.y > sheet.height + slack {
                outside.push(format!(
                    "{} (x {:.0}..{:.0}, y {:.0}..{:.0} mm)",
                    v.name,
                    lo.x * MM_PER_M,
                    hi.x * MM_PER_M,
                    lo.y * MM_PER_M,
                    hi.y * MM_PER_M
                ));
            }
        }
    }
    let views = if views.is_empty() {
        "no views yet".to_string()
    } else {
        format!("{} view(s): {}", views.len(), views.join(", "))
    };
    (views, outside)
}

/// The reply to add_sheet / activate_sheet / edit_sheet.
pub fn reply_text(request: &proto::SetupSheetRequest, reply: &proto::SetupSheetResponse) -> String {
    let Some(sheet) = &reply.sheet else {
        return format!("done; the drawing's sheets: {}", reply.sheet_names.join(", "));
    };
    let setup = files::sheet_text(sheet, &reply.sheet_format_path);
    let mut text = if reply.added {
        format!("added {setup}; it's now the active sheet")
    } else if request.activate {
        format!("{} is now the active sheet: {setup}", sheet.name)
    } else {
        let active = if sheet.active { " (the active sheet)".to_string() } else { format!("; the active sheet is still {}", reply.active_sheet) };
        format!("set up {setup}{active}")
    };
    let (views, outside) = views_text(sheet);
    text += &format!(". It has {views}. The drawing's sheets: {}.", reply.sheet_names.join(", "));

    let mut notes = Vec::new();
    if !outside.is_empty() {
        notes.push(format!(
            "these views reach past the sheet's edges (sheet {}): {} -- move them, give a smaller \
             scale or a bigger paper size",
            files::sheet_size_text(sheet),
            outside.join(", ")
        ));
    }
    if request.paper_size.is_some() && request.sheet_format.is_empty() && !reply.sheet_format_path.is_empty() {
        notes.push(format!(
            "the {} was kept; if it was drawn for another paper size, pass sheet_format for one that fits",
            files::sheet_format_text(&reply.sheet_format_path)
        ));
    }
    if request.first_angle.is_some() && sheet.views.iter().any(|v| v.r#type == PROJECTED_VIEW) {
        notes.push(
            "projected views stayed where they are and now show what this projection puts there \
             (inspect_drawing shows which way each one looks); dimensions and center marks in \
             them may be left on edges they no longer show -- check with render_drawing"
                .to_string(),
        );
    }
    if !notes.is_empty() {
        text += &format!("\nNote: {}.", notes.join("; "));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_parse_as_ratios() {
        assert_eq!(parse_scale("1:2").unwrap(), (1.0, 2.0));
        assert_eq!(parse_scale(" 2 : 1 ").unwrap(), (2.0, 1.0));
        assert_eq!(parse_scale("1:2.5").unwrap(), (1.0, 2.5));
        for bad in ["1/2", "0:1", "1:0", "-1:2", "a:b", "1:2:3", "", "inf:1"] {
            assert!(parse_scale(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn add_sends_setup_and_name() {
        let params: AddSheetParams = serde_json::from_value(serde_json::json!({
            "name": " Details ", "paper_size": "A4", "sheet_format": "a4 - iso", "scale": "1:5",
            "projection": "first_angle"
        }))
        .unwrap();
        let r = add_request(&params).unwrap();
        assert!(r.add && !r.activate);
        assert_eq!(
            (r.sheet_name.as_str(), r.paper_size, r.sheet_format.as_str(), r.scale_numerator, r.scale_denominator, r.first_angle),
            ("Details", Some(6), "a4 - iso", Some(1.0), 5.0, Some(true))
        );
        // Nothing given: a copy of the active sheet with SolidWorks' next name.
        let r = add_request(&AddSheetParams::default()).unwrap();
        assert_eq!(r, proto::SetupSheetRequest { add: true, ..Default::default() });
        let r = add_request(&AddSheetParams { paper_size_mm: Some([500.0, 250.0]), ..Default::default() }).unwrap();
        assert_eq!((r.paper_size, r.width, r.height), (Some(12), 0.5, 0.25));
        assert!(add_request(&AddSheetParams { scale: Some("2".into()), ..Default::default() }).is_err());
        assert!(add_request(&AddSheetParams { sheet_format: Some("C:\\x\\a.drwdot".into()), ..Default::default() }).is_err());
    }

    #[test]
    fn edit_needs_a_change() {
        assert!(edit_request(&EditSheetParams { sheet: Some("Sheet2".into()), ..Default::default() }).is_err());
        assert!(edit_request(&EditSheetParams { rename: Some(" ".into()), ..Default::default() }).is_err());
        let r = edit_request(&EditSheetParams { projection: Some(Projection::ThirdAngle), ..Default::default() }).unwrap();
        assert_eq!((r.sheet_name.as_str(), r.first_angle, r.add), ("", Some(false), false));
        let r = edit_request(&EditSheetParams { sheet: Some("Sheet2".into()), rename: Some("Parts list".into()), ..Default::default() })
            .unwrap();
        assert_eq!((r.sheet_name.as_str(), r.new_name.as_str()), ("Sheet2", "Parts list"));
        assert!(activate_request(&ActivateSheetParams { sheet: "  ".into() }).is_err());
        assert!(activate_request(&ActivateSheetParams { sheet: "Sheet2".into() }).unwrap().activate);
    }

    fn view(name: &str, kind: i32, sheet_scale: bool, min: [f64; 2], max: [f64; 2]) -> proto::ViewInfo {
        let v = |p: [f64; 2]| Some(proto::Vec3 { x: p[0], y: p[1], z: 0.0 });
        proto::ViewInfo {
            name: name.into(),
            r#type: kind,
            scale_numerator: 1.0,
            scale_denominator: 2.0,
            uses_sheet_scale: sheet_scale,
            base_view: if kind == PROJECTED_VIEW { "Drawing View1".into() } else { String::new() },
            outline: Some(proto::Box3 { min: v(min), max: v(max) }),
            ..Default::default()
        }
    }

    #[test]
    fn reply_flags_views_off_the_sheet_and_kept_formats() {
        let request = proto::SetupSheetRequest { paper_size: Some(6), first_angle: Some(true), ..Default::default() };
        let reply = proto::SetupSheetResponse {
            sheet: Some(proto::SheetInfo {
                name: "Sheet1".into(),
                active: true,
                paper_size: 6,
                width: 0.297,
                height: 0.21,
                scale_numerator: 1.0,
                scale_denominator: 2.0,
                first_angle: true,
                views: vec![
                    view("Drawing View1", 7, true, [0.05, 0.05], [0.1, 0.1]),
                    view("Drawing View2", PROJECTED_VIEW, false, [0.05, 0.2], [0.1, 0.25]),
                ],
                ..Default::default()
            }),
            sheet_format_path: r"C:\formats\a3 - iso.slddrt".into(),
            added: false,
            sheet_names: vec!["Sheet1".into(), "Sheet2".into()],
            active_sheet: "Sheet1".into(),
        };
        let text = reply_text(&request, &reply);
        assert!(
            text.starts_with(
                "set up Sheet1, A4 landscape (297 x 210 mm), sheet format a3 - iso, scale 1:2, \
                 first-angle projection (the active sheet). It has 2 view(s): Drawing View1 at 1:2 \
                 (sheet scale), Drawing View2 at 1:2 (projected from Drawing View1). The drawing's \
                 sheets: Sheet1, Sheet2."
            ),
            "{text}"
        );
        assert!(text.contains("Drawing View2 (x 50..100, y 200..250 mm)") && !text.contains("Drawing View1 (x"), "{text}");
        assert!(text.contains("sheet format a3 - iso was kept"), "{text}");
        assert!(text.contains("projected views stayed"), "{text}");

        // Another sheet changed in the background, nothing to flag.
        let request = proto::SetupSheetRequest { sheet_name: "Sheet2".into(), scale_numerator: Some(1.0), scale_denominator: 1.0, ..Default::default() };
        let mut reply = reply;
        reply.sheet = Some(proto::SheetInfo { name: "Sheet2".into(), active: false, views: vec![], ..reply.sheet.unwrap() });
        let text = reply_text(&request, &reply);
        assert!(text.contains("; the active sheet is still Sheet1. It has no views yet.") && !text.contains("Note"), "{text}");
    }
}

// DrawingAnnotations.cpp : Drawing annotations: SetupDrawingAnnotation (add a note, change a note or any
// annotation), DeleteDrawingAnnotations (see docs/notes/drawings.md, "Notes and leaders").
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "AnnotationHelpers.h"
#include "DrawingHelpers.h"

using namespace sld_mcp::service;

namespace
{
	constexpr int kMaxDeletedAnnotations = 500;

	// Positions closer than this are where they were asked to go: meters.
	constexpr double kPositionSlack = 1e-6;

	const char* OwnerWord(sld_mcp::v1::AnnotationOwner owner)
	{
		switch (owner)
		{
		case sld_mcp::v1::ANNOTATION_OWNER_VIEW:
			return "view";
		case sld_mcp::v1::ANNOTATION_OWNER_SHEET:
			return "sheet";
		case sld_mcp::v1::ANNOTATION_OWNER_SHEET_FORMAT:
			return "sheet format";
		default:
			return "document";
		}
	}

	std::string AnnotationName(IAnnotation* annotation)
	{
		ATL::CComBSTR name;
		annotation->GetName(&name);
		return ToUtf8(name);
	}

	// A dimension's full name ("D1@Sketch1@block.Part"); "" for other
	// annotations.
	std::string DimensionFullNameOf(IAnnotation* annotation)
	{
		long type = 0;
		ATL::CComPtr<IDispatch> specific;
		if (FAILED(annotation->GetType(&type)) || type != swDisplayDimension ||
			FAILED(annotation->GetSpecificAnnotation(&specific)) || !specific)
			return {};
		ATL::CComQIPtr<IDisplayDimension> display(specific.p);
		ATL::CComPtr<IDimension> dim;
		ATL::CComBSTR full;
		if (!display || FAILED(display->GetDimension2(0, &dim)) || !dim || FAILED(dim->get_FullName(&full)))
			return {};
		return ToUtf8(full);
	}

	// An annotation found by name, with the sheet and (for a view's) the
	// view it's listed under.
	struct FoundAnnotation
	{
		ATL::CComPtr<IAnnotation> annotation;
		std::string sheetName;
		ATL::CComPtr<IView> view;
		std::string fullName;  // a dimension's

		std::string Where() const
		{
			return (fullName.empty() ? std::string() : fullName + " in ") +
				(view ? ViewName(view) + " on " : std::string()) + sheetName;
		}
	};

	// The annotation called `name` (as IAnnotation::GetName has it, or a
	// dimension's full name, with or without the trailing "@<document>":
	// model dimensions of two features share short names like "D1"), looked
	// for in every view and on every sheet (the sheet's own list holds its
	// sheet format's too), or only those `sheetName` / `viewName` say.
	grpc::Status FindAnnotation(const std::vector<DrawingSheet>& sheets, const std::string& name,
		const std::string& sheetName, const std::string& viewName, FoundAnnotation& out)
	{
		std::vector<FoundAnnotation> hits;
		std::set<IUnknown*> seen;
		std::vector<ATL::CComPtr<IUnknown>> alive;
		auto consider = [&](const DrawingSheet& sheet, IView* view, IView* listedBy) {
			for (const auto& annotation : ViewAnnotationList(listedBy))
			{
				const std::string fullName = DimensionFullNameOf(annotation);
				if (AnnotationName(annotation) != name && fullName != name &&
					fullName.compare(0, name.size() + 1, name + "@") != 0)
					continue;
				ATL::CComPtr<IUnknown> identity;
				if (FAILED(annotation->QueryInterface(IID_PPV_ARGS(&identity))) || !seen.insert(identity.p).second)
					continue;
				alive.push_back(identity);
				hits.push_back({ annotation, sheet.name, view, fullName });
			}
		};
		for (const auto& sheet : sheets)
		{
			if (!sheetName.empty() && !EqualsIgnoringCase(sheet.name, sheetName))
				continue;
			for (const auto& view : sheet.views)
				if (viewName.empty() || ViewName(view) == viewName)
					consider(sheet, view, view);
			if (viewName.empty() && sheet.sheetView)
				consider(sheet, nullptr, sheet.sheetView);
		}
		const std::string scope = !viewName.empty() ? " in " + viewName : !sheetName.empty() ? " on " + sheetName : "";
		if (hits.empty())
			return grpc::Status(grpc::StatusCode::NOT_FOUND, "the drawing has no annotation named \"" + name + "\"" +
				scope + " -- inspect_drawing_annotations lists them");
		if (hits.size() > 1)
		{
			std::string where;
			std::set<std::string> fullNames;
			for (const auto& hit : hits)
			{
				where += (where.empty() ? "" : ", ") + hit.Where();
				fullNames.insert(hit.fullName);
			}
			// Dimensions with different full names: the full name tells them
			// apart; one full name in several views: the view does.
			const bool byFullName = fullNames.size() == hits.size() && !fullNames.count("");
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "several annotations are named \"" + name +
				"\" (" + where + ") -- " + (byFullName ? "give the dimension's full name (e.g. " + hits[0].fullName + ")"
					: "say which sheet or view"));
		}
		out = hits[0];
		return grpc::Status::OK;
	}

	ATL::CComPtr<INote> NoteOf(IAnnotation* annotation)
	{
		long type = 0;
		ATL::CComPtr<IDispatch> specific;
		if (FAILED(annotation->GetType(&type)) || type != swNote || FAILED(annotation->GetSpecificAnnotation(&specific)))
			return {};
		ATL::CComQIPtr<INote> note(specific.p);
		return ATL::CComPtr<INote>(note);
	}

	bool ReadPosition(IAnnotation* annotation, Eigen::Vector3d& out)
	{
		ATL::CComVariant positionVar;
		if (FAILED(annotation->GetPosition(&positionVar)))
			return false;
		const std::vector<double> p = comvariant::ToDoubles(positionVar);
		if (p.size() < 3)
			return false;
		out = Eigen::Vector3d(p[0], p[1], p[2]);
		return out.allFinite();
	}

	std::vector<std::string> LayerNames(IModelDoc2* doc)
	{
		std::vector<std::string> names;
		ATL::CComPtr<IDispatch> disp;
		if (FAILED(doc->GetLayerManager(&disp)) || !disp)
			return names;
		ATL::CComQIPtr<ILayerMgr> layers(disp.p);
		ATL::CComVariant list;
		if (layers && SUCCEEDED(layers->GetLayerList(&list)))
			for (const auto& name : comvariant::ToBstrs(list))
				names.push_back(ToUtf8(name));
		return names;
	}

	// What can be put back after a failed change.
	struct AnnotationState
	{
		Eigen::Vector3d position = Eigen::Vector3d::Zero();
		bool hasPosition = false;
		std::string layer;
		// Notes
		ATL::CComBSTR text;
		bool documentFont = true;
		ATL::CComPtr<IDispatch> textFormat;
		long justification = 0;
		double angle = 0.0;
		long border = 0;
		long borderSize = 0;
		long leaderStyle = 0;
		bool hasLeader = false;
		bool smartArrows = true;
		std::vector<long> arrows;
	};

	AnnotationState ReadState(IAnnotation* annotation, INote* note)
	{
		AnnotationState s;
		s.hasPosition = ReadPosition(annotation, s.position);
		ATL::CComBSTR layer;
		if (SUCCEEDED(annotation->get_Layer(&layer)))
			s.layer = ToUtf8(layer);
		if (!note)
			return s;
		note->GetText(&s.text);
		VARIANT_BOOL flag = VARIANT_TRUE;
		if (SUCCEEDED(annotation->GetUseDocTextFormat(0, &flag)))
			s.documentFont = flag != VARIANT_FALSE;
		annotation->GetTextFormat(0, &s.textFormat);
		note->GetTextJustification(&s.justification);
		note->get_Angle(&s.angle);
		note->GetBalloonStyle(&s.border);
		note->GetBalloonSize(&s.borderSize);
		flag = VARIANT_FALSE;
		s.hasLeader = SUCCEEDED(annotation->GetLeader(&flag)) && flag != VARIANT_FALSE;
		annotation->GetLeaderStyle(&s.leaderStyle);
		flag = VARIANT_TRUE;
		if (SUCCEEDED(annotation->GetSmartArrowHeadStyle(&flag)))
			s.smartArrows = flag != VARIANT_FALSE;
		long count = 0;
		annotation->GetLeaderCount(&count);
		for (long i = 0; i < count && i < kMaxAnnotationLeaders; ++i)
		{
			long arrow = swCLOSED_ARROWHEAD;
			annotation->GetArrowHeadStyleAtIndex(i, &arrow);
			s.arrows.push_back(arrow);
		}
		return s;
	}

	// swLeaderStyle_e without the attachment flags (top, center, ...).
	long BaseLeaderStyle(long style)
	{
		return style & 0xFF;
	}

	// What to change on an annotation; unset = keep. Notes only, except
	// position, layer and the view. (Leaders are made with the note:
	// SetLeaderAttachmentPointAtIndex detaches a leader from its entity,
	// seen 2026-10-22.)
	struct AnnotationChange
	{
		std::optional<std::string> text;
		std::optional<Eigen::Vector2d> position;  // sheet meters
		std::optional<long> leaderStyle;
		std::optional<long> arrow;
		sld_mcp::v1::NoteStyle style;
		std::optional<std::string> layer;
		ATL::CComPtr<IView> attachToView;
	};

	// Applies `c` step by step, each read back; stops at the first that
	// fails. The view comes last: it can't be taken back.
	grpc::Status ApplyChange(IModelDoc2* doc, IDrawingDoc* drawing, IModelDocExtension* ext, IAnnotation* annotation,
		INote* note, const AnnotationChange& c)
	{
		const std::string name = AnnotationName(annotation);

		if (c.text && note)
		{
			VARIANT_BOOL ok = VARIANT_FALSE;
			if (FAILED(note->SetText(FromUtf8(*c.text), &ok)) || !ok)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't take " + name + "'s new text");
		}

		const sld_mcp::v1::NoteStyle& s = c.style;
		if (note && (s.has_text_height() || s.has_font() || s.has_bold() || s.has_italic() || s.has_underline()))
		{
			ATL::CComPtr<IDispatch> formatDisp;
			ATL::CComQIPtr<ITextFormat> format;
			if (SUCCEEDED(annotation->GetTextFormat(0, &formatDisp)) && formatDisp)
				format = formatDisp.p;
			if (!format)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks gave no text format for " + name);
			if (s.has_text_height())
				format->put_CharHeight(s.text_height());
			if (s.has_font())
				format->put_TypeFaceName(FromUtf8(s.font()));
			if (s.has_bold())
				format->put_Bold(s.bold() ? VARIANT_TRUE : VARIANT_FALSE);
			if (s.has_italic())
				format->put_Italic(s.italic() ? VARIANT_TRUE : VARIANT_FALSE);
			if (s.has_underline())
				format->put_Underline(s.underline() ? VARIANT_TRUE : VARIANT_FALSE);
			VARIANT_BOOL ok = VARIANT_FALSE;
			annotation->SetTextFormat(0, VARIANT_FALSE, formatDisp, &ok);
			// Read back.
			ATL::CComPtr<IDispatch> afterDisp;
			annotation->GetTextFormat(0, &afterDisp);
			ATL::CComQIPtr<ITextFormat> after(afterDisp.p);
			double height = 0.0;
			ATL::CComBSTR font;
			VARIANT_BOOL bold = VARIANT_FALSE, italic = VARIANT_FALSE, underline = VARIANT_FALSE;
			if (after)
			{
				after->get_CharHeight(&height);
				after->get_TypeFaceName(&font);
				after->get_Bold(&bold);
				after->get_Italic(&italic);
				after->get_Underline(&underline);
			}
			if (!after || !ok)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't change " + name + "'s font");
			if (s.has_text_height() && std::abs(height - s.text_height()) > 1e-7)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't take that text height for " + name);
			if (s.has_font() && !EqualsIgnoringCase(ToUtf8(font), s.font()))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"SolidWorks didn't take the font \"" + s.font() + "\" (is it installed?); " + name + " is in " + ToUtf8(font));
			if ((s.has_bold() && (bold != VARIANT_FALSE) != s.bold()) ||
				(s.has_italic() && (italic != VARIANT_FALSE) != s.italic()) ||
				(s.has_underline() && (underline != VARIANT_FALSE) != s.underline()))
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks didn't change " + name + "'s bold / italic / underline");
		}
		if (note && s.has_justification())
		{
			note->SetTextJustification(s.justification());
			long now = 0;
			if (FAILED(note->GetTextJustification(&now)) || now != s.justification())
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't change " + name + "'s justification");
		}
		if (note && s.has_angle())
		{
			note->put_Angle(s.angle());
			double now = 0.0;
			if (FAILED(note->get_Angle(&now)) || std::abs(std::remainder(now - s.angle(), 2.0 * M_PI)) > 1e-6)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't turn " + name);
		}
		if (note && s.has_border())
		{
			VARIANT_BOOL ok = VARIANT_FALSE;
			note->SetBalloon(s.border(), s.has_border_size() ? s.border_size() : static_cast<long>(swBF_Tightest), &ok);
			long now = -1;
			if (!ok || FAILED(note->GetBalloonStyle(&now)) || now != s.border())
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't change " + name + "'s border");
		}

		// A note's angle is part of its text format: the document's font
		// turns it back to 0, and turning the text gives the note its own
		// font again (seen 2026-10-22); ValidateSetup refuses both together.
		if (note && s.document_font())
		{
			ATL::CComPtr<IDispatch> format;
			annotation->GetTextFormat(0, &format);
			VARIANT_BOOL ok = VARIANT_FALSE, useDoc = VARIANT_FALSE;
			annotation->SetTextFormat(0, VARIANT_TRUE, format, &ok);
			if (FAILED(annotation->GetUseDocTextFormat(0, &useDoc)) || !useDoc)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks didn't put " + name + " back on the document's font");
		}

		// Leader style and arrows. Per-leader arrows only show with smart
		// arrows off.
		if (note && (c.leaderStyle || c.arrow))
		{
			long current = 0;
			annotation->GetLeaderStyle(&current);
			VARIANT_BOOL leader = VARIANT_FALSE, smart = VARIANT_TRUE;
			annotation->GetLeader(&leader);
			annotation->GetSmartArrowHeadStyle(&smart);
			const long style = c.leaderStyle.value_or(leader ? BaseLeaderStyle(current) : static_cast<long>(swNO_LEADER));
			long result = 0;
			annotation->SetLeader3(style, swLS_SMART, c.arrow ? VARIANT_FALSE : smart, VARIANT_FALSE, VARIANT_FALSE,
				VARIANT_FALSE, &result);
			long after = 0;
			leader = VARIANT_FALSE;
			annotation->GetLeaderStyle(&after);
			annotation->GetLeader(&leader);
			const long afterBase = leader ? BaseLeaderStyle(after) : static_cast<long>(swNO_LEADER);
			if (c.leaderStyle && afterBase != *c.leaderStyle)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't change " + name +
					"'s leader style (SetLeader3 -> " + std::to_string(result) + ")");
			if (c.arrow)
			{
				long count = 0;
				annotation->GetLeaderCount(&count);
				for (long i = 0; i < count; ++i)
				{
					long ignored = 0, now = -1;
					annotation->SetArrowHeadStyleAtIndex(i, *c.arrow, &ignored);
					if (FAILED(annotation->GetArrowHeadStyleAtIndex(i, &now)) || now != *c.arrow)
						return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
							"SolidWorks didn't change the arrow of " + name + "'s leader " + std::to_string(i + 1));
				}
			}
		}

		if (c.layer)
		{
			annotation->put_Layer(FromUtf8(*c.layer));
			ATL::CComBSTR now;
			annotation->get_Layer(&now);
			if (!EqualsIgnoringCase(ToUtf8(now), *c.layer))
			{
				std::string known;
				for (const auto& layer : LayerNames(doc))
					known += (known.empty() ? "" : ", ") + layer;
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "SolidWorks didn't put " + name +
					" on layer \"" + *c.layer + "\" (the drawing's layers: " + (known.empty() ? "none" : known) + ")");
			}
		}

		// The position last but the view: the steps above may move the text.
		if (c.position)
		{
			Eigen::Vector3d now;
			const double z = ReadPosition(annotation, now) ? now.z() : 0.0;
			VARIANT_BOOL ok = VARIANT_FALSE;
			annotation->SetPosition2(c.position->x(), c.position->y(), z, &ok);
			if (!ReadPosition(annotation, now) || (now.head<2>() - *c.position).norm() > kPositionSlack)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't move " + name);
		}

		if (c.attachToView)
		{
			// A view's annotation into another view (SetupDrawingAnnotation
			// makes a sheet note again instead): it and the view selected,
			// then AttachAnnotation -- never with the annotation alone, which
			// faults inside SolidWorks (seen 2026-10-22). The text stays where
			// it is on the paper.
			const std::string viewName = ViewName(c.attachToView);
			auto attached = [&]() {
				std::string owner;
				return AnnotationOwnerOf(annotation, &owner) == sld_mcp::v1::ANNOTATION_OWNER_VIEW && owner == viewName;
			};
			if (!attached())
			{
				Eigen::Vector3d before;
				const bool had = ReadPosition(annotation, before);
				ATL::CComPtr<ISelectionMgr> selections;
				ATL::CComPtr<ISelectData> data;
				if (SUCCEEDED(doc->get_ISelectionManager(&selections)) && selections)
					selections->CreateSelectData(&data);
				doc->ClearSelection2(VARIANT_TRUE);
				VARIANT_BOOL noteSelected = VARIANT_FALSE, viewSelected = VARIANT_FALSE, ok = VARIANT_FALSE;
				annotation->Select3(VARIANT_FALSE, data, &noteSelected);
				ext->SelectByID2(FromUtf8(viewName), ATL::CComBSTR(L"DRAWINGVIEW"), 0.0, 0.0, 0.0, VARIANT_TRUE, 0,
					nullptr, swSelectOptionDefault, &viewSelected);
				long count = 0;
				if (selections)
					selections->GetSelectedObjectCount2(-1, &count);
				if (noteSelected && viewSelected && count == 2)
					drawing->AttachAnnotation(swAttachAnnotationOption_View, &ok);
				doc->ClearSelection2(VARIANT_TRUE);
				if (!attached())
					return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't move " + name +
						" into " + viewName + (count == 2 ? "" : " (it and the view couldn't both be selected)"));
				Eigen::Vector3d now;
				if (had && ReadPosition(annotation, now) && (now - before).head<2>().norm() > kPositionSlack)
					annotation->SetPosition2(before.x(), before.y(), before.z(), &ok);
			}
		}
		return grpc::Status::OK;
	}

	// Puts back what ApplyChange may have changed (not the view). Best effort.
	void RestoreState(IAnnotation* annotation, INote* note, const AnnotationState& s)
	{
		VARIANT_BOOL ok = VARIANT_FALSE;
		if (note)
		{
			note->SetText(s.text, &ok);
			annotation->SetTextFormat(0, s.documentFont ? VARIANT_TRUE : VARIANT_FALSE, s.textFormat, &ok);
			note->SetTextJustification(s.justification);
			note->put_Angle(s.angle);
			note->SetBalloon(s.border, s.borderSize, &ok);
			long result = 0;
			annotation->SetLeader3(s.hasLeader ? BaseLeaderStyle(s.leaderStyle) : static_cast<long>(swNO_LEADER),
				swLS_SMART, s.smartArrows ? VARIANT_TRUE : VARIANT_FALSE, VARIANT_FALSE, VARIANT_FALSE, VARIANT_FALSE,
				&result);
			long count = 0;
			annotation->GetLeaderCount(&count);
			for (long i = 0; i < count && static_cast<size_t>(i) < s.arrows.size(); ++i)
				annotation->SetArrowHeadStyleAtIndex(i, s.arrows[static_cast<size_t>(i)], &result);
		}
		annotation->put_Layer(FromUtf8(s.layer));
		if (s.hasPosition)
			annotation->SetPosition2(s.position.x(), s.position.y(), s.position.z(), &ok);
	}

	// Adds a view entity to the selection, picked at `tip` (sheet meters):
	// a note's leader to it then gets its arrow there and stays attached
	// (moving the arrow afterwards with SetLeaderAttachmentPointAtIndex
	// detaches it; seen 2026-10-22). Without a tip, or if that doesn't
	// select it, the entity alone: SolidWorks then puts the arrow at one of
	// its ends. False if neither selects it.
	bool SelectLeaderEntity(IModelDoc2* doc, DrawingViewContext& ctx, const ViewEntityItem& item,
		const std::optional<Eigen::Vector2d>& tip)
	{
		if (tip && SelectViewEntityAt(doc, ctx.view, item, tip, 0))
			return true;
		return SelectViewEntity(ctx.view, item);
	}

	// The display dimension called `name` in a view of the drawing (as
	// inspect_drawing lists it: "RD1@Drawing View1", or a model dimension's
	// full name), with its view. NOT_FOUND / INVALID_ARGUMENT (several).
	grpc::Status FindViewDimension(const std::vector<DrawingSheet>& sheets, const std::string& name,
		ATL::CComPtr<IAnnotation>& annotation, ATL::CComPtr<IView>& view, std::string& sheetName)
	{
		std::vector<std::string> where;
		for (const auto& sheet : sheets)
			for (const auto& v : sheet.views)
				for (const auto& a : ViewAnnotationList(v))
				{
					long type = 0;
					ATL::CComPtr<IDispatch> specific;
					if (FAILED(a->GetType(&type)) || type != swDisplayDimension ||
						FAILED(a->GetSpecificAnnotation(&specific)) || !specific)
						continue;
					ATL::CComQIPtr<IDisplayDimension> display(specific.p);
					ATL::CComPtr<IDimension> dim;
					ATL::CComBSTR full;
					if (!display || FAILED(display->GetDimension2(0, &dim)) || !dim || FAILED(dim->get_FullName(&full)))
						continue;
					// "RD1@Drawing View1" names "RD1@Drawing View1@Draw1.Drawing".
					const std::string fullName = ToUtf8(full);
					if (fullName != name && fullName.compare(0, name.size() + 1, name + "@") != 0)
						continue;
					where.push_back(ViewName(v));
					annotation = a;
					view = v;
					sheetName = sheet.name;
				}
		if (where.empty())
			return grpc::Status(grpc::StatusCode::NOT_FOUND, "no view of the drawing shows a dimension named \"" + name +
				"\" -- inspect_drawing with the view lists them (e.g. RD1@Drawing View1)");
		if (where.size() > 1)
		{
			std::string list;
			for (const auto& w : where)
				list += (list.empty() ? "" : ", ") + w;
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "\"" + name + "\" is shown in several views (" + list +
				") -- give the drawing's own name of the one you mean");
		}
		return grpc::Status::OK;
	}

	// Every annotation in the drawing's views and on its sheets.
	std::vector<ATL::CComPtr<IAnnotation>> DrawingAnnotationList(const std::vector<DrawingSheet>& sheets)
	{
		std::vector<ATL::CComPtr<IAnnotation>> all;
		for (const auto& sheet : sheets)
		{
			for (const auto& view : sheet.views)
				for (const auto& a : ViewAnnotationList(view))
					all.push_back(a);
			if (sheet.sheetView)
				for (const auto& a : ViewAnnotationList(sheet.sheetView))
					all.push_back(a);
		}
		return all;
	}

	// Gives a note made to replace another the old one's look: text format,
	// justification, angle, border, layer, leader style and arrows.
	void CopyLook(const AnnotationState& from, IAnnotation* to, INote* note)
	{
		VARIANT_BOOL ok = VARIANT_FALSE;
		to->SetTextFormat(0, from.documentFont ? VARIANT_TRUE : VARIANT_FALSE, from.textFormat, &ok);
		note->SetTextJustification(from.justification);
		note->put_Angle(from.angle);
		note->SetBalloon(from.border, from.borderSize, &ok);
		to->put_Layer(FromUtf8(from.layer));
		long result = 0;
		to->SetLeader3(from.hasLeader ? BaseLeaderStyle(from.leaderStyle) : static_cast<long>(swNO_LEADER), swLS_SMART,
			from.smartArrows ? VARIANT_TRUE : VARIANT_FALSE, VARIANT_FALSE, VARIANT_FALSE, VARIANT_FALSE, &result);
		if (!from.smartArrows && !from.arrows.empty())
		{
			long count = 0;
			to->GetLeaderCount(&count);
			for (long i = 0; i < count; ++i)
				to->SetArrowHeadStyleAtIndex(i, from.arrows[std::min(static_cast<size_t>(i), from.arrows.size() - 1)], &result);
		}
	}

	bool HasNoteFields(const sld_mcp::v1::SetupDrawingAnnotationRequest& r)
	{
		const auto& s = r.style();
		return r.has_text() || r.leader_to_size() > 0 || r.arrow_points_size() > 0 || r.has_leader_style() ||
			r.has_arrow() || s.has_text_height() || s.has_font() || s.has_bold() || s.has_italic() ||
			s.has_underline() || s.document_font() || s.has_justification() || s.has_angle() || s.has_border();
	}

	grpc::Status ValidateSetup(const sld_mcp::v1::SetupDrawingAnnotationRequest& r)
	{
		const bool create = r.name().empty();
		const auto& s = r.style();
		if (r.has_gdt())
		{
			if (!create)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a symbol can only be added");
			const bool styled = s.has_text_height() || s.has_font() || s.has_bold() || s.has_italic() ||
				s.has_underline() || s.document_font() || s.has_justification() || s.has_angle() || s.has_border();
			if (r.has_text() || r.has_leader_style() || r.has_arrow() || styled)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"text, style and leader settings are for notes, not symbols");
			if (r.leader_to_size() > 1)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a symbol goes on one entity");
			if (r.leader_to_size() > 0 && !r.attach_to_dimension().empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "attach it to an entity or a dimension, not both");
			// InsertDatumTag2 with a dimension selected makes a datum symbol
			// attached to nothing, off the view (seen 2026-10-24).
			if (r.gdt().has_datum() && (r.leader_to_size() == 0 || !r.attach_to_dimension().empty()))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a datum symbol goes on a view entity");
			if (r.gdt().has_weld() && r.leader_to_size() == 0)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a weld symbol goes on a view entity");
			if (r.gdt().surface_finish().no_leader() && r.leader_to_size() == 0)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a surface finish symbol without a leader stands on a view entity");
			if (!r.gdt().has_gtol() && !r.attach_to_dimension().empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "only feature control frames go on dimensions");
			if (!r.attach_to_dimension().empty() && r.owner() != sld_mcp::v1::ANNOTATION_OWNER_UNSPECIFIED &&
				r.owner() != sld_mcp::v1::ANNOTATION_OWNER_VIEW)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "on a dimension it belongs to the dimension's view");
			if (grpc::Status status = ValidateGdtSymbol(r.gdt()); !status.ok())
				return status;
		}
		else if (!r.attach_to_dimension().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "only feature control frames go on dimensions");
		if (create && !r.has_text() && !r.has_gdt())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a new note needs its text");
		if (create && !r.attach_to_view().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a new note goes in a view by view_name");
		if (!create && r.owner() != sld_mcp::v1::ANNOTATION_OWNER_UNSPECIFIED)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"an annotation can only be moved into a view (attach_to_view), not to a sheet or sheet format");
		if (r.has_position() && r.has_move_by())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give position or move_by, not both");
		if (r.has_move_by() && (!std::isfinite(r.move_by().x()) || !std::isfinite(r.move_by().y())))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "move_by must be finite");
		if (r.leader_to_size() > kMaxAnnotationLeaders)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"at most " + std::to_string(kMaxAnnotationLeaders) + " leaders");
		for (const auto& ref : r.leader_to())
			if (ref.view_name() != r.leader_to(0).view_name() || ref.view_name().empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a note's leaders all go to entities of one view");
		if (r.arrow_points_size() > 0 && r.arrow_points_size() != r.leader_to_size())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"give one arrow point per leader_to entity (arrows are placed with their leaders)");
		if (r.has_leader_style() && (r.leader_style() < swNO_LEADER || r.leader_style() > swUNDERLINED))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown leader style " + std::to_string(r.leader_style()));
		if (r.has_arrow() && (r.arrow() < swOPEN_ARROWHEAD || r.arrow() > swSMART_ARROWHEAD))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown arrow style " + std::to_string(r.arrow()));
		if (s.has_text_height() && !(s.text_height() > 0.0 && s.text_height() < 1.0))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the text height must be above 0 (meters)");
		if (s.document_font() && (s.has_text_height() || s.has_font() || s.has_bold() || s.has_italic() || s.has_underline()))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "document_font replaces the font settings; give one or the other");
		if (s.document_font() && s.has_angle() && std::abs(s.angle()) > 1e-9)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"a turned note has its own font: the document's font sets the angle back to 0");
		if (s.has_justification() && (s.justification() < swTextJustificationLeft || s.justification() > swTextJustificationRight))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown justification " + std::to_string(s.justification()));
		if (s.has_angle() && !std::isfinite(s.angle()))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the angle must be finite");
		if (s.has_border() && (s.border() < swBS_None || s.border() > swBS_Verbose))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown border " + std::to_string(s.border()));
		if (s.has_border_size() && (s.border_size() < swBF_Tightest || s.border_size() > swBF_5Chars))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "unknown border size " + std::to_string(s.border_size()));
		return grpc::Status::OK;
	}

	using SetupRequest = sld_mcp::v1::SetupDrawingAnnotationRequest;

	// Where SetupDrawingAnnotation's annotation is, or goes. Looked up
	// before anything changes.
	struct AnnotationTarget
	{
		sld_mcp::v1::AnnotationOwner owner = sld_mcp::v1::ANNOTATION_OWNER_UNSPECIFIED;
		std::string sheetName;
		ATL::CComPtr<IView> ownerView;
		FoundAnnotation found;                  // the annotation changed
		ATL::CComPtr<INote> note;               // its note, or the new one
		ATL::CComPtr<IAnnotation> onDimension;  // gdt on a dimension
		ATL::CComPtr<IAnnotation> annotation;   // what the change goes to: found's, or the new one
	};

	// The leaders asked for: their arrow points (sheet meters), and the view
	// of their entities with the entities.
	struct Leaders
	{
		std::vector<Eigen::Vector2d> tips;
		std::unique_ptr<DrawingViewContext> ctx;
		std::vector<ViewEntityItem> items;
	};

	// A new annotation's owner, sheet and view: from view_name, the
	// dimension a frame goes on and the view of the leaders' entities, which
	// must agree. A sheet note goes on the active sheet unless sheet_name says.
	grpc::Status ResolveNewTarget(const SetupRequest& r, const ActiveDrawing& d, AnnotationTarget& t)
	{
		const std::vector<DrawingSheet>& sheets = d.sheets;
		t.owner = r.owner();
		std::string viewName = r.view_name();
		if (!r.attach_to_dimension().empty())
		{
			ATL::CComPtr<IView> dimensionView;
			std::string dimensionSheet;
			if (grpc::Status status = FindViewDimension(sheets, r.attach_to_dimension(), t.onDimension, dimensionView,
				dimensionSheet); !status.ok())
				return status;
			const std::string dimensionViewName = ViewName(dimensionView);
			if (!viewName.empty() && viewName != dimensionViewName)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, r.attach_to_dimension() + " is in " +
					dimensionViewName + ", so what goes on it belongs to " + dimensionViewName + ", not " + viewName);
			viewName = dimensionViewName;
		}
		if (r.has_gdt())
			if (grpc::Status status = CheckDatumLabelFree(r.gdt(), DatumLabels(DrawingAnnotationList(sheets)));
				!status.ok())
				return status;
		if (r.leader_to_size() > 0)
		{
			const std::string& leaderView = r.leader_to(0).view_name();
			if (t.owner != sld_mcp::v1::ANNOTATION_OWNER_UNSPECIFIED && t.owner != sld_mcp::v1::ANNOTATION_OWNER_VIEW)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, std::string("a note with leaders to ") +
					leaderView + "'s entities belongs to that view, not to the " + OwnerWord(t.owner));
			if (!viewName.empty() && viewName != leaderView)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "the leaders go to " + leaderView +
					"'s entities, so the note belongs to " + leaderView + ", not " + viewName);
			viewName = leaderView;
		}
		if (t.owner == sld_mcp::v1::ANNOTATION_OWNER_UNSPECIFIED)
			t.owner = viewName.empty() ? sld_mcp::v1::ANNOTATION_OWNER_SHEET : sld_mcp::v1::ANNOTATION_OWNER_VIEW;
		if (t.owner == sld_mcp::v1::ANNOTATION_OWNER_VIEW)
		{
			if (viewName.empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "say which view the note goes in");
			if (grpc::Status status = FindDrawingView(sheets, viewName, t.ownerView, t.sheetName); !status.ok())
				return status;
			if (!r.sheet_name().empty() && !EqualsIgnoringCase(r.sheet_name(), t.sheetName))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, viewName + " is on " + t.sheetName + ", not " +
					r.sheet_name());
			return grpc::Status::OK;
		}
		if (!viewName.empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, std::string("a ") + OwnerWord(t.owner) +
				" note doesn't go in a view (" + viewName + ")");
		const std::string& wanted = r.sheet_name().empty() ? d.activeSheet : r.sheet_name();
		const DrawingSheet* sheet = FindSheet(sheets, wanted);
		if (!sheet)
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"the drawing has no sheet named \"" + wanted + "\" (its sheets: " + SheetList(sheets) + ")");
		t.sheetName = sheet->name;
		return grpc::Status::OK;
	}

	// The annotation r.name() names, with its owner and note. Refuses note
	// settings for other annotations, and leaders to the entities of another
	// view than the one it's in after the change.
	grpc::Status ResolveExistingTarget(const SetupRequest& r, const std::vector<DrawingSheet>& sheets,
		AnnotationTarget& t)
	{
		if (grpc::Status status = FindAnnotation(sheets, r.name(), r.sheet_name(), r.view_name(), t.found); !status.ok())
			return status;
		t.sheetName = t.found.sheetName;
		t.owner = AnnotationOwnerOf(t.found.annotation, nullptr);
		t.note = NoteOf(t.found.annotation);
		if (!t.note && HasNoteFields(r))
		{
			long type = 0;
			t.found.annotation->GetType(&type);
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, r.name() + " isn't a note (annotation type " +
				std::to_string(type) + "): only its position, layer and view can be changed");
		}
		if (r.leader_to_size() > 0)
		{
			std::string ownerName;
			AnnotationOwnerOf(t.found.annotation, &ownerName);
			const std::string& leaderView = r.leader_to(0).view_name();
			const std::string target = !r.attach_to_view().empty() ? r.attach_to_view()
				: t.owner == sld_mcp::v1::ANNOTATION_OWNER_VIEW ? ownerName : std::string();
			if (target != leaderView)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, r.name() + " is " +
					(target.empty() ? std::string("on the ") + OwnerWord(t.owner) : "in " + target) +
					": its leaders can only go to entities of the view it's in (attach_to_view " + leaderView +
					" puts it there)");
		}
		return grpc::Status::OK;
	}

	// The change asked for, positions in sheet meters; `tips` = the arrow
	// points, one per leader.
	grpc::Status ReadChange(const SetupRequest& r, const ActiveDrawing& d, const std::string& sheetName,
		AnnotationChange& change, std::vector<Eigen::Vector2d>& tips)
	{
		const std::vector<DrawingSheet>& sheets = d.sheets;
		IMathUtility* const math = d.math;
		if (r.has_text())
			change.text = r.text();
		if (r.has_position())
		{
			Eigen::Vector2d at;
			if (grpc::Status status = ToSheetPoint(sheets, math, r.position(), "position", at); !status.ok())
				return status;
			change.position = at;
		}
		for (int i = 0; i < r.arrow_points_size(); ++i)
		{
			Eigen::Vector2d at;
			if (grpc::Status status = ToSheetPoint(sheets, math, r.arrow_points(i),
				"arrow point " + std::to_string(i + 1), at); !status.ok())
				return status;
			tips.push_back(at);
		}
		if (r.has_leader_style())
			change.leaderStyle = r.leader_style();
		if (r.has_arrow())
			change.arrow = r.arrow();
		change.style = r.style();
		if (r.has_layer())
			change.layer = r.layer();
		if (!r.attach_to_view().empty())
		{
			std::string viewSheet;
			if (grpc::Status status = FindDrawingView(sheets, r.attach_to_view(), change.attachToView, viewSheet);
				!status.ok())
				return status;
			if (viewSheet != sheetName)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, r.name() + " is on " + sheetName + " and " +
					r.attach_to_view() + " on " + viewSheet + ": an annotation can only go into a view of its own sheet");
		}
		return grpc::Status::OK;
	}

	// The view of the leaders' entities, with what resolving them needs.
	grpc::Status MakeLeaderContext(const std::string& viewName, const ActiveDrawing& d,
		sld_mcp::v1::SetupDrawingAnnotationResponse* response, std::unique_ptr<DrawingViewContext>& ctx)
	{
		ctx = std::make_unique<DrawingViewContext>();
		if (grpc::Status status = FindDrawingView(d.sheets, viewName, ctx->view, ctx->sheetName); !status.ok())
			return status;
		ctx->model = ViewModelOf(ctx->view);
		ctx->ids = std::make_unique<PersistIds>(d.ext);
		ctx->modelIds = std::make_unique<PersistIds>(ctx->model.ext);
		ctx->components = ViewComponents(ctx->view, ctx->model);
		if (ctx->model.type == swDocASSEMBLY)
			ctx->componentList = std::make_unique<ComponentList>(*ctx->modelIds, d.math,
				response->mutable_components(), ATL::CComQIPtr<IAssemblyDoc>(ctx->model.doc.p));
		return grpc::Status::OK;
	}

	// SolidWorks puts a new annotation in the active view (none: the sheet;
	// the format while it's being edited) and gives it a leader to each
	// entity selected, its arrow where the entity was picked. So: activates
	// the owner's view and selects each leader's entity (at its tip) and the
	// dimension a frame goes on. (ActivateView, even of no view, ends
	// editing the sheet format.)
	grpc::Status SelectAttachments(const ActiveDrawing& d, const SetupRequest& r, const AnnotationTarget& t,
		const Leaders& leaders)
	{
		IModelDoc2* const doc = d.doc;
		VARIANT_BOOL ok = VARIANT_FALSE;
		if (t.owner == sld_mcp::v1::ANNOTATION_OWNER_VIEW)
			d.drawing->ActivateView(FromUtf8(ViewName(t.ownerView)), &ok);
		else if (t.owner == sld_mcp::v1::ANNOTATION_OWNER_SHEET)
			d.drawing->ActivateView(ATL::CComBSTR(L""), &ok);
		doc->ClearSelection2(VARIANT_TRUE);
		for (size_t i = 0; i < leaders.items.size(); ++i)
		{
			std::optional<Eigen::Vector2d> tip;
			if (i < leaders.tips.size())
				tip = leaders.tips[i];
			if (!SelectLeaderEntity(doc, *leaders.ctx, leaders.items[i], tip))
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks wouldn't select leader " +
					std::to_string(i + 1) + "'s entity in " + ViewName(leaders.ctx->view));
			}
		}
		if (t.onDimension)
		{
			VARIANT_BOOL selected = VARIANT_FALSE;
			if (FAILED(t.onDimension->Select3(VARIANT_TRUE, nullptr, &selected)) || !selected)
			{
				doc->ClearSelection2(VARIANT_TRUE);
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks wouldn't select " + r.attach_to_dimension());
			}
		}
		return grpc::Status::OK;
	}

	// Inserts the symbol, or the note (the change's text, else the old
	// note's), on the selection, and clears it; it becomes `t.annotation`. A
	// note uses up `change.text` and becomes `t.note`.
	grpc::Status InsertOnSelection(const ActiveDrawing& d, const SetupRequest& r, const AnnotationState& before,
		const std::vector<Eigen::Vector2d>& tips, AnnotationTarget& t, AnnotationChange& change)
	{
		IModelDoc2* const doc = d.doc;
		ATL::CComPtr<IAnnotation>& annotation = t.annotation;
		if (r.has_gdt())
		{
			// A frame / datum symbol goes on what's selected, like a note's
			// leaders. A surface finish symbol is made where it goes; standing
			// on its entity (no position) that's the arrow point.
			const Eigen::Vector2d insertAt = change.position ? *change.position
				: !tips.empty() ? tips[0] : Eigen::Vector2d(Eigen::Vector2d::Zero());
			grpc::Status status = InsertGdtSymbol(doc, d.ext, r.gdt(), Eigen::Vector3d(insertAt.x(), insertAt.y(), 0.0), annotation);
			doc->ClearSelection2(VARIANT_TRUE);
			return status;
		}
		const ATL::CComBSTR text = change.text ? FromUtf8(*change.text) : before.text;
		ATL::CComPtr<IDispatch> noteDisp;
		HRESULT hr = doc->InsertNote(text, &noteDisp);
		doc->ClearSelection2(VARIANT_TRUE);
		if (FAILED(hr))
			return ComError("IModelDoc2::InsertNote", hr);
		ATL::CComQIPtr<INote> made(noteDisp.p);
		ATL::CComPtr<IDispatch> annotationDisp;
		if (made && SUCCEEDED(made->GetAnnotation(&annotationDisp)) && annotationDisp)
			annotation = ATL::CComQIPtr<IAnnotation>(annotationDisp.p);
		if (!annotation)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't add the note");
		t.note = made;
		change.text.reset();
		return grpc::Status::OK;
	}

	// The new annotation went where it belongs and attached as asked (a
	// note's leaders to every entity, a symbol to its entity or dimension);
	// otherwise it's deleted again.
	grpc::Status CheckInserted(const ActiveDrawing& d, const SetupRequest& r, const AnnotationTarget& t, size_t leaders,
		bool gdtAttaches)
	{
		IModelDoc2* const doc = d.doc;
		IModelDocExtension* const ext = d.ext;
		IAnnotation* const annotation = t.annotation;
		std::string ownerName;
		const auto actual = AnnotationOwnerOf(annotation, &ownerName);
		const std::string expectedName = t.owner == sld_mcp::v1::ANNOTATION_OWNER_VIEW ? ViewName(t.ownerView)
			: t.owner == sld_mcp::v1::ANNOTATION_OWNER_SHEET ? t.sheetName : ownerName;
		if (actual != t.owner || ownerName != expectedName)
		{
			DeleteAnnotations(doc, ext, { annotation });
			auto place = [](sld_mcp::v1::AnnotationOwner o, const std::string& name) {
				return std::string("the ") + OwnerWord(o) +
					(o == sld_mcp::v1::ANNOTATION_OWNER_SHEET_FORMAT ? std::string() : " " + name);
			};
			return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks put the " + std::string(r.has_gdt()
				? "symbol" : "note") + " in " + place(actual, ownerName) + " instead of " + place(t.owner, expectedName) +
				" (on " + t.sheetName + "); it was deleted again");
		}
		if (r.has_gdt())
		{
			// A datum symbol has no leader of its own (its triangle sits on
			// the entity); a frame on a dimension none either.
			if (gdtAttaches && !GdtAttached(annotation))
			{
				DeleteAnnotations(doc, ext, { annotation });
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't attach the symbol to " +
					(t.onDimension ? r.attach_to_dimension() : std::string("the entity")) + "; it was deleted again");
			}
			return grpc::Status::OK;
		}
		long count = 0;
		annotation->GetLeaderCount(&count);
		if (count != static_cast<long>(leaders))
		{
			DeleteAnnotations(doc, ext, { annotation });
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks gave the note " +
				std::to_string(count) + " leader(s) for " + std::to_string(leaders) + " entities; it was deleted again");
		}
		return grpc::Status::OK;
	}

	// Where a new note given no position goes: above its view, or the
	// sheet's middle.
	std::optional<Eigen::Vector2d> DefaultPosition(IView* ownerView, const std::vector<DrawingSheet>& sheets,
		const std::string& sheetName)
	{
		ATL::CComVariant outline;
		std::vector<double> o;
		if (ownerView && SUCCEEDED(ownerView->GetOutline(&outline)))
			o = comvariant::ToDoubles(outline);
		if (o.size() >= 4)
			return Eigen::Vector2d(o[0], o[3] + 0.010);
		SheetSetup setup;
		if (const DrawingSheet* sheet = FindSheet(sheets, sheetName); sheet && ReadSheetSetup(sheet->sheet, setup).ok())
			return Eigen::Vector2d(setup.width / 2, setup.height / 2);
		return std::nullopt;
	}

	// Makes t.annotation: the new annotation or, for an existing one
	// (t.found), the one replacing it (with its look and place), in the
	// owner's view, sheet or format. On error nothing new is left.
	grpc::Status MakeAnnotation(const ActiveDrawing& d, const SetupRequest& r, const Leaders& leaders,
		const AnnotationState& before, AnnotationTarget& t, AnnotationChange& change)
	{
		if (grpc::Status status = SelectAttachments(d, r, t, leaders); !status.ok())
			return status;
		if (grpc::Status status = InsertOnSelection(d, r, before, leaders.tips, t, change); !status.ok())
			return status;
		const bool gdtAttaches = r.has_gdt() && (!leaders.items.empty() || t.onDimension);
		if (grpc::Status status = CheckInserted(d, r, t, leaders.items.size(), gdtAttaches); !status.ok())
			return status;
		if (t.found.annotation)
		{
			CopyLook(before, t.annotation, t.note);
			if (!change.position && before.hasPosition)
				change.position = before.position.head<2>();
		}
		else if (!change.position && !gdtAttaches)
			change.position = DefaultPosition(t.ownerView, d.sheets, t.sheetName);
		return grpc::Status::OK;
	}

	// The reply's annotation, described with the ids inspect_drawing gives
	// (the drawing's, and the owner view's assembly's components), and its
	// sheet.
	void DescribeSetup(const ActiveDrawing& d, const AnnotationTarget& t, Leaders& leaders,
		sld_mcp::v1::SetupDrawingAnnotationResponse* response)
	{
		IAnnotation* const annotation = t.annotation;
		PersistIds ids(d.ext);
		std::string ownerName;
		std::unique_ptr<PersistIds> modelIds;
		std::unique_ptr<ComponentList> components;
		if (leaders.ctx && leaders.ctx->componentList)
			components = std::move(leaders.ctx->componentList);
		else if (AnnotationOwnerOf(annotation, &ownerName) == sld_mcp::v1::ANNOTATION_OWNER_VIEW)
			for (const auto& s : d.sheets)
				for (const auto& v : s.views)
					if (ViewName(v) == ownerName)
						if (const ViewModel model = ViewModelOf(v); model.type == swDocASSEMBLY)
						{
							modelIds = std::make_unique<PersistIds>(model.ext);
							components = std::make_unique<ComponentList>(*modelIds, d.math, response->mutable_components(),
								ATL::CComQIPtr<IAssemblyDoc>(model.doc.p));
						}
		DescribeAnnotation(annotation, ids, components.get(), response->mutable_annotation());
		for (const auto& s : DrawingSheets(d.drawing))
			if (s.name == t.sheetName)
			{
				DrawingBudget budget;
				DescribeSheet(s, d.activeSheet, d.ext, d.math, {}, false, false, budget, response->mutable_sheet());
			}
	}
}

grpc::Status SolidWorksService::SetupDrawingAnnotation(grpc::ServerContext* context,
	const sld_mcp::v1::SetupDrawingAnnotationRequest* request,
	sld_mcp::v1::SetupDrawingAnnotationResponse* response)
{
	if (grpc::Status status = ValidateSetup(*request); !status.ok())
		return status;
	const bool create = request->name().empty();

	return m_executor.Execute(context, "SetupDrawingAnnotation", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		// Everything is looked up before anything changes.
		ActiveDrawing d;
		if (grpc::Status status = GetActiveDrawing(swApp, d); !status.ok())
			return status;
		IModelDoc2* const doc = d.doc;
		IModelDocExtension* const ext = d.ext;
		AnnotationTarget t;
		if (grpc::Status status = create ? ResolveNewTarget(*request, d, t)
			: ResolveExistingTarget(*request, d.sheets, t); !status.ok())
			return status;
		AnnotationChange change;
		Leaders leaders;
		if (grpc::Status status = ReadChange(*request, d, t.sheetName, change, leaders.tips); !status.ok())
			return status;

		// Leaders' entities, resolved and selected while the view's sheet is
		// showing.
		if (request->leader_to_size() > 0)
		{
			if (grpc::Status status = MakeLeaderContext(request->leader_to(0).view_name(), d, response, leaders.ctx);
				!status.ok())
				return status;
			if (!create)
				t.ownerView = leaders.ctx->view;
		}
		// A note's leaders are made with it, and SolidWorks moves a view's
		// annotation into another view but not one of the sheet or its format
		// (AttachAnnotation says true and leaves it, seen 2026-10-22): new
		// leaders, or a sheet (format) note going into a view, mean a new note
		// in the old one's place, which is then deleted.
		const bool intoView = change.attachToView && t.owner != sld_mcp::v1::ANNOTATION_OWNER_VIEW;
		if (intoView && !t.note)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, request->name() + " belongs to the " +
				OwnerWord(t.owner) + ": SolidWorks only moves a view's annotations into another view (a note of the " +
				OwnerWord(t.owner) + " is made again in the view)");
		const bool remake = !create && (request->leader_to_size() > 0 || intoView);
		if (remake)
		{
			if (!t.ownerView)
				t.ownerView = change.attachToView;
			change.attachToView.Release();
			t.owner = sld_mcp::v1::ANNOTATION_OWNER_VIEW;
		}

		{
			SheetActivation activation(d.drawing);
			if (!activation.Show(t.sheetName))
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "couldn't switch to sheet " + t.sheetName);
			EditModeScope editMode(d.drawing);
			// A sheet-format annotation is edited in the format, the rest on the
			// sheet; EditSheet also leaves any active view.
			editMode.Set(t.owner == sld_mcp::v1::ANNOTATION_OWNER_SHEET_FORMAT);

			if (leaders.ctx)
			{
				VARIANT_BOOL ok = VARIANT_FALSE;
				d.drawing->ActivateView(FromUtf8(ViewName(leaders.ctx->view)), &ok);
				for (int i = 0; i < request->leader_to_size(); ++i)
				{
					ViewEntityItem item;
					if (grpc::Status status = ResolveDrawingRef(*leaders.ctx, request->leader_to(i),
						"leader " + std::to_string(i + 1), item); !status.ok())
						return status;
					leaders.items.push_back(item);
				}
			}

			AnnotationState before;
			if (!create)
				before = ReadState(t.found.annotation, t.note);
			if (intoView && request->leader_to_size() == 0 && before.hasLeader && !before.arrows.empty())
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, request->name() +
					" has leaders: going into a view it's made again, and they can't come along -- give attach_to "
					"(entities of that view) for new ones, or leader none first");
			if (request->has_move_by())
			{
				if (!before.hasPosition)
					return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
						"SolidWorks didn't say where " + request->name() + " is; give a position instead");
				change.position = before.position.head<2>() + Eigen::Vector2d(request->move_by().x(), request->move_by().y());
			}

			if (create || remake)
			{
				if (grpc::Status status = MakeAnnotation(d, *request, leaders, before, t, change); !status.ok())
					return status;
			}
			else
				t.annotation = t.found.annotation;
			IAnnotation* const annotation = t.annotation;

			grpc::Status status = ApplyChange(doc, d.drawing, ext, annotation, t.note, change);
			if (status.ok() && remake && !DeleteAnnotations(doc, ext, { t.found.annotation }))
				status = grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks didn't delete " + request->name() + " after making its replacement with the new leaders");
			doc->ClearSelection2(VARIANT_TRUE);
			if (!status.ok())
			{
				if (create || remake)
					DeleteAnnotations(doc, ext, { annotation });
				else
					RestoreState(annotation, t.note, before);
				doc->GraphicsRedraw2();
				return status;
			}
			doc->GraphicsRedraw2();
		}

		DescribeSetup(d, t, leaders, response);
		response->set_created(create);
		if (remake)
			response->set_replaced(request->name());
		response->set_active_sheet(d.activeSheet);
		return grpc::Status::OK;
	});
}

grpc::Status SolidWorksService::DeleteDrawingAnnotations(grpc::ServerContext* context,
	const sld_mcp::v1::DeleteDrawingAnnotationsRequest* request,
	sld_mcp::v1::DeleteDrawingAnnotationsResponse* response)
{
	if (request->names_size() == 0 || request->names_size() > kMaxDeletedAnnotations)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"give between 1 and " + std::to_string(kMaxDeletedAnnotations) + " annotation names");

	return m_executor.Execute(context, "DeleteDrawingAnnotations", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		ATL::CComQIPtr<IDrawingDoc> drawing;
		long modelType = swDocNONE;
		if (grpc::Status status = GetActiveDrawing(swApp, doc, drawing); !status.ok())
		{
			// A part's or assembly's annotations (PMI).
			doc.Release();
			if (!GetActiveModel(swApp, doc, modelType).ok())
				return status;
		}
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;

		if (!drawing)
		{
			if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
				return status;
			auto byName = [&](const std::string& name) {
				bool truncated = false;
				std::vector<ATL::CComPtr<IAnnotation>> hits;
				for (const auto& annotation : AllAnnotations(doc, ext, truncated))
					if (AnnotationName(annotation) == name)
						hits.push_back(annotation);
				return hits;
			};
			std::vector<ATL::CComPtr<IAnnotation>> list;
			std::vector<std::string> names;
			PersistIds ids(ext);
			for (const auto& name : request->names())
			{
				const auto hits = byName(name);
				if (hits.empty())
					return grpc::Status(grpc::StatusCode::NOT_FOUND, "the active " + std::string(modelType == swDocPART
						? "part" : "assembly") + " has no annotation named \"" + name + "\" -- inspect_annotations lists them");
				if (std::find(names.begin(), names.end(), name) != names.end())
					continue;
				names.push_back(name);
				for (const auto& hit : hits)
				{
					list.push_back(hit);
					DescribeAnnotation(hit, ids, nullptr, response->add_deleted());
				}
			}
			DeleteAnnotations(doc, ext, list);
			for (const auto& name : names)
				if (!byName(name).empty())
					return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't delete " + name);
			doc->GraphicsRedraw2();
			return grpc::Status::OK;
		}

		// All found first, then deleted sheet by sheet (annotations are only
		// selected on the sheet showing).
		const std::vector<DrawingSheet> sheets = DrawingSheets(drawing);
		std::map<std::string, std::vector<ATL::CComPtr<IAnnotation>>> bySheet;
		std::vector<std::string> sheetOrder;
		PersistIds ids(ext);
		for (const auto& name : request->names())
		{
			FoundAnnotation found;
			if (grpc::Status status = FindAnnotation(sheets, name, request->sheet_name(), request->view_name(), found);
				!status.ok())
				return status;
			auto& list = bySheet[found.sheetName];
			if (list.empty())
				sheetOrder.push_back(found.sheetName);
			if (std::any_of(list.begin(), list.end(), [&](const ATL::CComPtr<IAnnotation>& a) {
				return SameObject(a, found.annotation); }))
				continue;
			list.push_back(found.annotation);
			DescribeAnnotation(found.annotation, ids, nullptr, response->add_deleted());
		}

		SheetActivation activation(drawing);
		for (const auto& sheetName : sheetOrder)
		{
			if (!activation.Show(sheetName))
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "couldn't switch to sheet " + sheetName);
			const auto& list = bySheet[sheetName];
			// Identities taken now: a deleted annotation's object may not answer
			// any more. (Not looked up again by name: another annotation of the
			// same short name, in another view, isn't this one.)
			std::map<IUnknown*, std::string> deleting;
			std::vector<ATL::CComPtr<IUnknown>> alive;
			for (const auto& annotation : list)
			{
				ATL::CComPtr<IUnknown> identity;
				if (SUCCEEDED(annotation->QueryInterface(IID_PPV_ARGS(&identity))))
				{
					const std::string fullName = DimensionFullNameOf(annotation);
					deleting[identity.p] = fullName.empty() ? AnnotationName(annotation) : fullName;
					alive.push_back(identity);
				}
			}
			DeleteAnnotations(doc, ext, list);
			// Gone?
			for (const auto& sheet : DrawingSheets(drawing))
			{
				if (sheet.name != sheetName)
					continue;
				std::vector<IView*> lists(sheet.views.begin(), sheet.views.end());
				if (sheet.sheetView)
					lists.push_back(sheet.sheetView);
				for (IView* view : lists)
					for (const auto& annotation : ViewAnnotationList(view))
					{
						ATL::CComPtr<IUnknown> identity;
						if (FAILED(annotation->QueryInterface(IID_PPV_ARGS(&identity))))
							continue;
						if (auto it = deleting.find(identity.p); it != deleting.end())
							return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't delete " + it->second +
								" (on " + sheetName + ")" + (sheetOrder.size() > 1 ? "; annotations on the sheets before it are deleted" : ""));
					}
			}
		}
		doc->GraphicsRedraw2();
		return grpc::Status::OK;
	});
}

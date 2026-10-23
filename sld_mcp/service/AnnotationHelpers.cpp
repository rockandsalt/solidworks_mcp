// AnnotationHelpers.cpp : see AnnotationHelpers.h.
#include "pch.h"
#include "AnnotationHelpers.h"
#include "Common.h"
#include "DimensionHelpers.h"
#include "AssemblyHelpers.h"

namespace sld_mcp::service
{
	bool InvokeByName(IDispatch* obj, const wchar_t* name, ATL::CComVariant& result)
	{
		result.Clear();
		if (!obj)
			return false;
		DISPID id = DISPID_UNKNOWN;
		LPOLESTR names[] = { const_cast<LPOLESTR>(name) };
		if (FAILED(obj->GetIDsOfNames(IID_NULL, names, 1, LOCALE_USER_DEFAULT, &id)))
			return false;
		DISPPARAMS none = { nullptr, nullptr, 0, 0 };
		return SUCCEEDED(obj->Invoke(id, IID_NULL, LOCALE_USER_DEFAULT,
			DISPATCH_METHOD | DISPATCH_PROPERTYGET, &none, &result, nullptr, nullptr));
	}

	std::vector<ATL::CComPtr<IAnnotation>> AllAnnotations(IModelDoc2* doc, IModelDocExtension* ext,
		bool& truncated)
	{
		std::vector<ATL::CComPtr<IAnnotation>> out;
		std::set<IUnknown*> seen;  // identities; `alive` keeps them from being reused
		std::vector<ATL::CComPtr<IUnknown>> alive;
		auto add = [&](IUnknown* obj) {
			ATL::CComQIPtr<IAnnotation> annotation(obj);
			if (!annotation)
				return;
			ATL::CComPtr<IUnknown> identity;
			if (FAILED(annotation->QueryInterface(IID_PPV_ARGS(&identity))) || !identity)
				return;
			if (!seen.insert(identity.p).second)
				return;
			alive.push_back(identity);
			if (out.size() >= kMaxAnnotations)
			{
				truncated = true;
				return;
			}
			out.push_back(ATL::CComPtr<IAnnotation>(annotation));
		};

		ATL::CComVariant all;
		if (SUCCEEDED(ext->GetAnnotations(&all)))
			for (const auto& disp : comvariant::ToDispatches(all))
				add(disp);

		ATL::CComVariant views;
		if (SUCCEEDED(ext->get_AnnotationViews(&views)))
			for (const auto& disp : comvariant::ToDispatches(views))
			{
				ATL::CComQIPtr<IAnnotationView> view(disp.p);
				ATL::CComVariant annotations;
				if (view && SUCCEEDED(view->get_Annotations(&annotations)))
					for (const auto& a : comvariant::ToDispatches(annotations))
						add(a);
			}

		ATL::CComPtr<IAnnotation> annotation;
		if (SUCCEEDED(doc->IGetFirstAnnotation2(&annotation)))
		{
			// Bounded in case the chain ever loops.
			for (size_t steps = 0; annotation && steps < 4 * kMaxAnnotations; ++steps)
			{
				add(annotation);
				ATL::CComPtr<IAnnotation> next;
				if (FAILED(annotation->GetNext3(&next)))
					break;
				annotation = next;
			}
		}
		return out;
	}

	void AddText(const ATL::CComBSTR& text, sld_mcp::v1::AnnotationInfo* info)
	{
		if (text.Length() > 0)
			info->add_text(ToUtf8(text));
	}

	void DescribeAttached(IDispatch* obj, PersistIds& ids, ComponentList* components,
		sld_mcp::v1::AttachedEntity* out)
	{
		auto kind = sld_mcp::v1::ATTACHED_KIND_OTHER;
		if (ATL::CComQIPtr<IFace2> face(obj); face)
			kind = sld_mcp::v1::ATTACHED_KIND_FACE;
		else if (ATL::CComQIPtr<IEdge> edge(obj); edge)
			kind = sld_mcp::v1::ATTACHED_KIND_EDGE;
		else if (ATL::CComQIPtr<IVertex> vertex(obj); vertex)
			kind = sld_mcp::v1::ATTACHED_KIND_VERTEX;
		else if (ATL::CComQIPtr<IFeature> feature(obj); feature)
			kind = sld_mcp::v1::ATTACHED_KIND_FEATURE;
		out->set_kind(kind);
		if (kind != sld_mcp::v1::ATTACHED_KIND_OTHER)
			out->set_persist_id(ids.Get(obj));
		if (components)
			out->set_component_persist_id(components->Add(ComponentOf(obj)));
	}

	void DescribeModelDimension(IDimension* dim, sld_mcp::v1::AnnotationDimension* out)
	{
		VARIANT_BOOL flag = VARIANT_FALSE;
		if (SUCCEEDED(dim->IsReference(&flag)))
			out->set_reference(flag != VARIANT_FALSE);

		ATL::CComPtr<IDimensionTolerance> tolerance;
		if (SUCCEEDED(dim->get_Tolerance(&tolerance)) && tolerance)
		{
			long type = swTolNONE;
			if (SUCCEEDED(tolerance->get_Type(&type)))
				out->set_tolerance_type(static_cast<int32_t>(type));
			double value = 0.0;
			if (SUCCEEDED(tolerance->GetMaxValue(&value)) && std::isfinite(value))
				out->set_tolerance_max(value);
			value = 0.0;
			if (SUCCEEDED(tolerance->GetMinValue(&value)) && std::isfinite(value))
				out->set_tolerance_min(value);
			ATL::CComBSTR fit;
			if (SUCCEEDED(tolerance->GetHoleFitValue(&fit)))
				out->set_hole_fit(ToUtf8(fit));
			fit.Empty();
			if (SUCCEEDED(tolerance->GetShaftFitValue(&fit)))
				out->set_shaft_fit(ToUtf8(fit));
		}

		ATL::CComPtr<IFeature> owner;
		if (SUCCEEDED(dim->GetFeatureOwner(&owner)) && owner)
		{
			out->set_owner_feature(FeatureName(owner));
			ATL::CComBSTR typeName;
			if (SUCCEEDED(owner->GetTypeName2(&typeName)))
				out->set_owner_feature_type(ToUtf8(typeName));
		}
	}

	void DescribeAnnotationDimension(IDisplayDimension* display, sld_mcp::v1::AnnotationDimension* out)
	{
		auto text = [&](long which) {
			ATL::CComBSTR s;
			display->GetText(which, &s);
			return ToUtf8(s);
		};
		out->set_prefix(text(swDimensionTextPrefix));
		out->set_suffix(text(swDimensionTextSuffix));
		out->set_callout_above(text(swDimensionTextCalloutAbove));
		out->set_callout_below(text(swDimensionTextCalloutBelow));

		VARIANT_BOOL flag = VARIANT_FALSE;
		if (SUCCEEDED(display->IsHoleCallout(&flag)))
			out->set_hole_callout(flag != VARIANT_FALSE);
		flag = VARIANT_FALSE;
		if (SUCCEEDED(display->get_Inspection(&flag)))
			out->set_inspection(flag != VARIANT_FALSE);

		ATL::CComPtr<IDimension> dim;
		if (FAILED(display->GetDimension2(0, &dim)) || !dim)
			return;
		DescribeDimension(display, out->mutable_dimension());
		DescribeModelDimension(dim, out);
	}

	void DescribeGtol(IGtol* gtol, sld_mcp::v1::AnnotationGtol* out)
	{
		// Reads one row of the frame; false if there's nothing in it.
		auto readFrame = [&](short frame) {
			ATL::CComVariant valuesVar, symbolsVar;
			std::vector<ATL::CComBSTR> values;
			std::vector<long long> symbols;
			if (SUCCEEDED(gtol->GetFrameValues(frame, &valuesVar)))
				values = comvariant::ToBstrs(valuesVar);
			if (SUCCEEDED(gtol->GetFrameSymbols(frame, &symbolsVar)))
				symbols = comvariant::ToInt64s(symbolsVar);
			const bool anyValue = std::any_of(values.begin(), values.end(),
				[](const ATL::CComBSTR& s) { return s.Length() > 0; });
			const bool anySymbol = !symbols.empty() && symbols[0] != 0 && symbols[0] != swGcsNONE;
			if (!anyValue && !anySymbol)
				return false;

			// Same order as IGtol::SetFrameValues / SetFrameSymbols:
			// values = tol1, tol2, datum1..3; symbols = GCS, tol1 diameter,
			// tol1 MC, tol2 diameter, tol2 MC, datum1..3 MC.
			auto value = [&](size_t i) { return i < values.size() ? ToUtf8(values[i]) : std::string(); };
			auto symbol = [&](size_t i, long long fallback) { return i < symbols.size() ? symbols[i] : fallback; };
			sld_mcp::v1::GtolFrame* f = out->add_frames();
			f->set_symbol(static_cast<int32_t>(symbol(0, -1)));
			f->set_tolerance1(value(0));
			f->set_tolerance2(value(1));
			f->set_tolerance1_diameter(symbol(1, 0) != 0);
			f->set_tolerance1_condition(static_cast<int32_t>(symbol(2, 0)));
			f->set_tolerance2_diameter(symbol(3, 0) != 0);
			f->set_tolerance2_condition(static_cast<int32_t>(symbol(4, 0)));
			for (size_t i = 0; i < 3; ++i)
			{
				f->add_datums(value(2 + i));
				f->add_datum_conditions(static_cast<int32_t>(symbol(5 + i, 0)));
			}
			return true;
		};

		short count = 0;
		if (FAILED(gtol->GetFrameCount(&count)) || count <= 0)
			count = 2;  // unknown: a frame has at most two rows in the classic API
		bool any = false;
		for (short frame = 1; frame <= count && frame <= kMaxGtolFrames; ++frame)
			any = readFrame(frame) || any;
		// Frame numbers are documented as 1-based; try 0 in case they aren't.
		if (!any)
			readFrame(0);

		ATL::CComBSTR datum;
		if (SUCCEEDED(gtol->GetDatumIdentifier(&datum)))
			out->set_datum_label(ToUtf8(datum));
		VARIANT_BOOL flag = VARIANT_FALSE;
		if (SUCCEEDED(gtol->GetCompositeFrame(&flag)))
			out->set_composite(flag != VARIANT_FALSE);
		flag = VARIANT_FALSE;
		if (SUCCEEDED(gtol->GetAllAround(&flag)))
			out->set_all_around(flag != VARIANT_FALSE);
		ATL::CComBSTR ptz;
		if (SUCCEEDED(gtol->GetPTZHeight(&ptz)))
			out->set_projected_zone_height(ToUtf8(ptz));
		long format = 0;
		if (SUCCEEDED(gtol->GetFormat(&format)))
			out->set_format(static_cast<int32_t>(format));
	}

	sld_mcp::v1::AnnotationOwner AnnotationOwnerOf(IAnnotation* annotation, std::string* ownerName)
	{
		long type = -1;
		if (FAILED(annotation->get_OwnerType(&type)))
			return sld_mcp::v1::ANNOTATION_OWNER_UNSPECIFIED;
		sld_mcp::v1::AnnotationOwner owner = sld_mcp::v1::ANNOTATION_OWNER_UNSPECIFIED;
		switch (type)
		{
		case swAnnotationOwner_DrawingView:
			owner = sld_mcp::v1::ANNOTATION_OWNER_VIEW;
			break;
		case swAnnotationOwner_DrawingSheet:
			owner = sld_mcp::v1::ANNOTATION_OWNER_SHEET;
			break;
		case swAnnotationOwner_DrawingTemplate:
			owner = sld_mcp::v1::ANNOTATION_OWNER_SHEET_FORMAT;
			break;
		default:
			return sld_mcp::v1::ANNOTATION_OWNER_UNSPECIFIED;
		}
		if (ownerName)
		{
			ownerName->clear();
			ATL::CComPtr<IDispatch> disp;
			if (SUCCEEDED(annotation->get_Owner(&disp)) && disp)
			{
				ATL::CComBSTR name;
				if (ATL::CComQIPtr<IView> view(disp.p); view && SUCCEEDED(view->GetName2(&name)))
					*ownerName = ToUtf8(name);
				else if (ATL::CComQIPtr<ISheet> sheet(disp.p); sheet && SUCCEEDED(sheet->GetName(&name)))
					*ownerName = ToUtf8(name);
				else
				{
					// The sheet format: neither of the above.
					ATL::CComVariant result;
					if ((InvokeByName(disp, L"GetName2", result) || InvokeByName(disp, L"GetName", result)) &&
						result.vt == VT_BSTR)
						*ownerName = ToUtf8(ATL::CComBSTR(result.bstrVal));
				}
			}
		}
		return owner;
	}

	void DescribeLeaders(IAnnotation* annotation, sld_mcp::v1::AnnotationInfo* info)
	{
		long style = 0;
		VARIANT_BOOL hasLeader = VARIANT_FALSE;
		if (SUCCEEDED(annotation->GetLeader(&hasLeader)) && hasLeader && SUCCEEDED(annotation->GetLeaderStyle(&style)))
			info->set_leader_style(static_cast<int32_t>(style));
		long count = 0;
		if (FAILED(annotation->GetLeaderCount(&count)))
			return;
		for (long i = 0; i < count && i < kMaxAnnotationLeaders; ++i)
		{
			sld_mcp::v1::AnnotationLeader* leader = info->add_leaders();
			ATL::CComVariant pointsVar;
			if (SUCCEEDED(annotation->GetLeaderPointsAtIndex(i, &pointsVar)))
			{
				const std::vector<double> p = comvariant::ToDoubles(pointsVar);
				for (size_t k = 0; k + 2 < p.size(); k += 3)
					SetVec3(leader->add_points(), &p[k]);
			}
			long arrow = -1;
			if (FAILED(annotation->GetArrowHeadStyleAtIndex(i, &arrow)))
				arrow = -1;
			leader->set_arrow(static_cast<int32_t>(arrow));
		}
	}

	void DescribeNote(INote* note, IAnnotation* annotation, sld_mcp::v1::NoteInfo* out)
	{
		ATL::CComPtr<IDispatch> formatDisp;
		if (SUCCEEDED(annotation->GetTextFormat(0, &formatDisp)) && formatDisp)
			if (ATL::CComQIPtr<ITextFormat> format(formatDisp.p); format)
			{
				double height = 0.0;
				if (SUCCEEDED(format->get_CharHeight(&height)))
					out->set_text_height(height);
				ATL::CComBSTR font;
				if (SUCCEEDED(format->get_TypeFaceName(&font)))
					out->set_font(ToUtf8(font));
				VARIANT_BOOL flag = VARIANT_FALSE;
				if (SUCCEEDED(format->get_Bold(&flag)))
					out->set_bold(flag != VARIANT_FALSE);
				flag = VARIANT_FALSE;
				if (SUCCEEDED(format->get_Italic(&flag)))
					out->set_italic(flag != VARIANT_FALSE);
				flag = VARIANT_FALSE;
				if (SUCCEEDED(format->get_Underline(&flag)))
					out->set_underline(flag != VARIANT_FALSE);
			}
		VARIANT_BOOL flag = VARIANT_FALSE;
		if (SUCCEEDED(annotation->GetUseDocTextFormat(0, &flag)))
			out->set_document_font(flag != VARIANT_FALSE);
		flag = VARIANT_FALSE;
		if (SUCCEEDED(annotation->GetSmartArrowHeadStyle(&flag)))
			out->set_smart_arrows(flag != VARIANT_FALSE);
		long value = 0;
		if (SUCCEEDED(note->GetTextJustification(&value)))
			out->set_justification(static_cast<int32_t>(value));
		double angle = 0.0;
		if (SUCCEEDED(note->get_Angle(&angle)) && std::isfinite(angle))
			out->set_angle(angle);
		value = 0;
		if (SUCCEEDED(note->GetBalloonStyle(&value)))
			out->set_border(static_cast<int32_t>(value));
		value = 0;
		if (SUCCEEDED(note->GetBalloonSize(&value)))
			out->set_border_size(static_cast<int32_t>(value));
		ATL::CComVariant extent;
		if (SUCCEEDED(note->GetExtent(&extent)))
			if (const std::vector<double> box = comvariant::ToDoubles(extent); box.size() >= 6)
				SetBox(out->mutable_extent(), box);
	}

	void DescribeAnnotation(IAnnotation* annotation, PersistIds& ids, ComponentList* components,
		sld_mcp::v1::AnnotationInfo* info)
	{
		std::string ownerName;
		info->set_owner(AnnotationOwnerOf(annotation, &ownerName));
		info->set_owner_name(ownerName);
		ATL::CComBSTR layer;
		if (SUCCEEDED(annotation->get_Layer(&layer)))
			info->set_layer(ToUtf8(layer));
		DescribeLeaders(annotation, info);

		ATL::CComBSTR name;
		if (SUCCEEDED(annotation->GetName(&name)))
			info->set_name(ToUtf8(name));
		long type = 0;
		if (SUCCEEDED(annotation->GetType(&type)))
			info->set_type(static_cast<int32_t>(type));
		long visibility = swAnnotationVisibilityUnknown;
		if (SUCCEEDED(annotation->get_Visible(&visibility)))
			info->set_visibility(static_cast<int32_t>(visibility));
		VARIANT_BOOL flag = VARIANT_FALSE;
		if (SUCCEEDED(annotation->IsDangling(&flag)))
			info->set_dangling(flag != VARIANT_FALSE);
		long pmiType = 0;
		if (SUCCEEDED(annotation->GetPMIType(&pmiType)))
			info->set_pmi_type(static_cast<int32_t>(pmiType));

		ATL::CComVariant positionVar;
		if (SUCCEEDED(annotation->GetPosition(&positionVar)))
			if (const std::vector<double> p = comvariant::ToDoubles(positionVar);
				p.size() >= 3 && std::isfinite(p[0]) && std::isfinite(p[1]) && std::isfinite(p[2]))
				SetVec3(info->mutable_position(), p.data());

		// Attached geometry, each entity once.
		std::set<std::string> attachedIds;
		auto attach = [&](IDispatch* obj) {
			if (!obj || info->attached_size() >= kMaxAttachedPerAnnotation)
				return;
			sld_mcp::v1::AttachedEntity entity;
			DescribeAttached(obj, ids, components, &entity);
			if (!entity.persist_id().empty() && !attachedIds.insert(entity.persist_id()).second)
				return;
			*info->add_attached() = std::move(entity);
		};
		ATL::CComVariant attachedVar;
		if (SUCCEEDED(annotation->GetAttachedEntities3(&attachedVar)))
			for (const auto& disp : comvariant::ToDispatches(attachedVar))
				attach(disp);
		attachedVar.Clear();
		if (info->attached_size() == 0 && SUCCEEDED(annotation->GetAttachedEntities2(&attachedVar)))
			for (const auto& disp : comvariant::ToDispatches(attachedVar))
				attach(disp);

		flag = VARIANT_FALSE;
		if (SUCCEEDED(annotation->IsDimXpert(&flag)) && flag != VARIANT_FALSE)
		{
			info->set_dimxpert(true);
			ATL::CComBSTR dimXpertName;
			if (SUCCEEDED(annotation->GetDimXpertName(&dimXpertName)))
				info->set_dimxpert_name(ToUtf8(dimXpertName));
			ATL::CComPtr<IDispatch> feature;
			if (SUCCEEDED(annotation->GetDimXpertFeature(&feature)) && feature)
			{
				ATL::CComVariant result;
				if (InvokeByName(feature, L"Name", result) && result.vt == VT_BSTR)
					info->set_dimxpert_feature(ToUtf8(ATL::CComBSTR(result.bstrVal)));
				// DimXpert annotations may report no attachments; their
				// feature knows its faces.
				if (info->attached_size() == 0 && InvokeByName(feature, L"GetFaces", result))
					for (const auto& disp : comvariant::ToDispatches(result))
						attach(disp);
			}
		}

		ATL::CComPtr<IDispatch> specific;
		if (FAILED(annotation->GetSpecificAnnotation(&specific)) || !specific)
			return;
		switch (type)
		{
		case swDisplayDimension:
			if (ATL::CComQIPtr<IDisplayDimension> display(specific.p); display)
			{
				DescribeAnnotationDimension(display, info->mutable_dimension());
				ATL::CComBSTR all;
				if (SUCCEEDED(display->GetText(swDimensionTextAll, &all)))
					AddText(all, info);
			}
			break;
		case swGTol:
			if (ATL::CComQIPtr<IGtol> gtol(specific.p); gtol)
			{
				DescribeGtol(gtol, info->mutable_gtol());
				AddIndexedTexts(gtol.p, info);
			}
			break;
		case swDatumTag:
			if (ATL::CComQIPtr<IDatumTag> datum(specific.p); datum)
			{
				ATL::CComBSTR label;
				if (SUCCEEDED(datum->GetLabel(&label)))
					info->set_datum_label(ToUtf8(label));
			}
			break;
		case swDatumTargetSym:
			if (ATL::CComQIPtr<IDatumTargetSym> target(specific.p); target)
				AddIndexedTexts(target.p, info);
			break;
		case swSFSymbol:
			if (ATL::CComQIPtr<ISFSymbol> finish(specific.p); finish)
				AddIndexedTexts(finish.p, info);
			break;
		case swNote:
			if (ATL::CComQIPtr<INote> note(specific.p); note)
			{
				ATL::CComBSTR text;
				if (SUCCEEDED(note->GetText(&text)))
					AddText(text, info);
				DescribeNote(note, annotation, info->mutable_note());
			}
			break;
		default:
			break;
		}
	}
}

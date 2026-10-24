// Interference.cpp : CheckInterference (see docs/notes/architecture.md, "Assemblies + mates").
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "DrawingHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// Most interferences one reply lists.
	constexpr int kMaxInterferences = 1000;

	// The assembly a CheckInterference request is about: the active one, or
	// the model of a view of the active drawing.
	struct CheckedAssembly
	{
		ATL::CComPtr<IModelDoc2> doc;
		ATL::CComPtr<IView> view;  // null: the active assembly itself
		ATL::CComPtr<IModelDoc2> active;  // the active document (the drawing, or `doc`)
	};

	// The view `name`, else the one assembly the drawing's views show.
	grpc::Status DrawingViewAssembly(IDrawingDoc* drawing, const std::string& name, CheckedAssembly& out)
	{
		const std::vector<DrawingSheet> sheets = DrawingSheets(drawing);
		if (!name.empty())
		{
			std::string sheetName;
			if (grpc::Status status = FindDrawingView(sheets, name, out.view, sheetName); !status.ok())
				return status;
			const ViewModel model = ViewModelOf(out.view);
			if (!model.doc)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					name + "'s model isn't loaded (resolve the view first)");
			if (model.type != swDocASSEMBLY)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					name + " shows a part, not an assembly -- interference is between an assembly's components");
			out.doc = model.doc;
			return grpc::Status::OK;
		}
		// Views of the same assembly (front, top, section, ...) count once.
		std::vector<std::pair<ATL::CComPtr<IModelDoc2>, std::vector<std::string>>> assemblies;
		for (const auto& sheet : sheets)
			for (const auto& view : sheet.views)
			{
				const ViewModel model = ViewModelOf(view);
				if (!model.doc || model.type != swDocASSEMBLY)
					continue;
				auto it = std::find_if(assemblies.begin(), assemblies.end(),
					[&](const auto& a) { return SameObject(a.first, model.doc); });
				if (it == assemblies.end())
				{
					assemblies.push_back({ model.doc, {} });
					it = std::prev(assemblies.end());
					if (!out.view)
						out.view = view;
				}
				it->second.push_back(ViewName(view));
			}
		if (assemblies.empty())
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"the drawing has no view of an assembly (or its model isn't loaded) -- interference is "
				"between an assembly's components");
		if (assemblies.size() > 1)
		{
			std::string list;
			for (const auto& [doc, views] : assemblies)
			{
				ATL::CComBSTR title;
				doc->GetTitle(&title);
				list += std::string(list.empty() ? "" : "; ") + ToUtf8(title) + ": ";
				for (size_t i = 0; i < views.size(); ++i)
					list += (i ? ", " : "") + views[i];
			}
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"the drawing shows " + std::to_string(assemblies.size()) + " assemblies -- give the view "
				"whose assembly to check (" + list + ")");
		}
		out.doc = assemblies[0].first;
		return grpc::Status::OK;
	}

	grpc::Status GetCheckedAssembly(ISldWorks* swApp, const std::string& viewName, CheckedAssembly& out)
	{
		ATL::CComPtr<IModelDoc2> active;
		HRESULT hr = swApp->get_IActiveDoc2(&active);
		if (FAILED(hr))
			return ComError("ISldWorks::IActiveDoc2", hr);
		if (!active)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"no document is open in SolidWorks -- open an assembly (or a drawing of one) first");
		out.active = active;
		long type = swDocNONE;
		if (FAILED(hr = active->GetType(&type)))
			return ComError("IModelDoc2::GetType", hr);
		if (type == swDocASSEMBLY)
		{
			if (!viewName.empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"a view only goes with a drawing; the active document is an assembly");
			if (grpc::Status status = CheckNoOpenSketch(active); !status.ok())
				return status;
			out.doc = active;
			return grpc::Status::OK;
		}
		if (type == swDocDRAWING)
		{
			ATL::CComQIPtr<IDrawingDoc> drawing(active.p);
			if (!drawing)
				return grpc::Status(grpc::StatusCode::INTERNAL, "active drawing has no IDrawingDoc interface");
			return DrawingViewAssembly(drawing, viewName, out);
		}
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"the active document is a part -- interference is between an assembly's components (open the "
			"assembly, or a drawing of it)");
	}

	std::string ActiveConfigurationName(IModelDoc2* doc)
	{
		ATL::CComPtr<IDispatch> disp;
		ATL::CComBSTR name;
		if (SUCCEEDED(doc->GetActiveConfiguration(&disp)) && disp)
			if (ATL::CComQIPtr<IConfiguration> config(disp.p); config)
				config->get_Name(&name);
		return ToUtf8(name);
	}

	// SolidWorks hands out no interference manager for a document loaded
	// only as a drawing's reference (not visible; seen 2026-10-24). This
	// shows it for the check and hides it again when it goes out of scope,
	// then makes `active` (the drawing) the active document again if it
	// isn't: showing the assembly activates it, hiding it activated the
	// drawing again by itself.
	class ShownForCheck
	{
	public:
		ShownForCheck(ISldWorks* swApp, IModelDoc2* doc, IModelDoc2* active) : m_swApp(swApp), m_doc(doc), m_active(active)
		{
			VARIANT_BOOL visible = VARIANT_TRUE;
			if (SUCCEEDED(m_doc->get_Visible(&visible)) && !visible)
				m_shown = SUCCEEDED(m_doc->put_Visible(VARIANT_TRUE));
		}
		~ShownForCheck()
		{
			if (!m_shown)
				return;
			m_doc->put_Visible(VARIANT_FALSE);
			ATL::CComPtr<IModelDoc2> now;
			if (SUCCEEDED(m_swApp->get_IActiveDoc2(&now)) && SameObject(now, m_active))
				return;
			ATL::CComBSTR title;
			m_active->GetTitle(&title);
			long errors = 0;
			ATL::CComPtr<IDispatch> activated;
			m_swApp->ActivateDoc3(title, VARIANT_FALSE, swDontRebuildActiveDoc, &errors, &activated);
		}
		ShownForCheck(const ShownForCheck&) = delete;
		ShownForCheck& operator=(const ShownForCheck&) = delete;

	private:
		ATL::CComPtr<ISldWorks> m_swApp;
		ATL::CComPtr<IModelDoc2> m_doc;
		ATL::CComPtr<IModelDoc2> m_active;
		bool m_shown = false;
	};

	// The manager's options as the request wants them, put back as they
	// were when this goes out of scope (the manager keeps them between
	// checks), then Done -- which leaves interference mode (interferences
	// shown red, other components wireframe) and marks the assembly
	// modified (seen 2026-10-24). Only the options that pick what is
	// checked: changing ShowIgnoredInterferences, MakeInterferingPartsTransparent
	// or NonInterferingComponentDisplay made SolidWorks drop the coincident
	// (touching) interferences, and the display goes back with Done anyway.
	class InterferenceSession
	{
	public:
		explicit InterferenceSession(IInterferenceDetectionMgr* mgr) : m_mgr(mgr)
		{
			m_mgr->get_TreatCoincidenceAsInterference(&m_coincidence);
			m_mgr->get_TreatSubAssembliesAsComponents(&m_subassemblies);
			m_mgr->get_IncludeMultibodyPartInterferences(&m_multibody);
			m_mgr->get_IgnoreHiddenBodies(&m_ignoreHidden);
		}
		~InterferenceSession()
		{
			m_mgr->put_TreatCoincidenceAsInterference(m_coincidence);
			m_mgr->put_TreatSubAssembliesAsComponents(m_subassemblies);
			m_mgr->put_IncludeMultibodyPartInterferences(m_multibody);
			m_mgr->put_IgnoreHiddenBodies(m_ignoreHidden);
			m_mgr->Done();
		}
		InterferenceSession(const InterferenceSession&) = delete;
		InterferenceSession& operator=(const InterferenceSession&) = delete;

		void Apply(const sld_mcp::v1::CheckInterferenceRequest& r)
		{
			auto b = [](bool v) { return v ? VARIANT_TRUE : VARIANT_FALSE; };
			m_mgr->put_TreatCoincidenceAsInterference(b(r.include_coincident()));
			m_mgr->put_TreatSubAssembliesAsComponents(b(r.treat_subassemblies_as_components()));
			m_mgr->put_IncludeMultibodyPartInterferences(b(r.include_multibody_parts()));
			m_mgr->put_IgnoreHiddenBodies(b(!r.include_hidden()));
		}

	private:
		ATL::CComPtr<IInterferenceDetectionMgr> m_mgr;
		VARIANT_BOOL m_coincidence = VARIANT_FALSE, m_subassemblies = VARIANT_FALSE, m_multibody = VARIANT_FALSE,
			m_ignoreHidden = VARIANT_FALSE;
	};
}

grpc::Status SolidWorksService::CheckInterference(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::CheckInterferenceRequest* request,
	sld_mcp::v1::CheckInterferenceResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	CheckedAssembly target;
	if (grpc::Status status = GetCheckedAssembly(swApp, request->view(), target); !status.ok())
		return status;
	ATL::CComQIPtr<IAssemblyDoc> assembly(target.doc.p);
	if (!assembly)
		return grpc::Status(grpc::StatusCode::INTERNAL, "the assembly has no IAssemblyDoc interface");
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(target.doc, ext); !status.ok())
		return status;
	ATL::CComQIPtr<IMathUtility> math;
	if (grpc::Status status = GetMathUtility(swApp, math); !status.ok())
		return status;
	PersistIds ids(ext);

	ATL::CComBSTR title, path;
	target.doc->GetTitle(&title);
	target.doc->GetPathName(&path);
	response->mutable_document()->set_title(ToUtf8(title));
	response->mutable_document()->set_type(sld_mcp::v1::DOCUMENT_TYPE_ASSEMBLY);
	response->mutable_document()->set_path(ToUtf8(path));
	response->set_configuration(ActiveConfigurationName(target.doc));
	if (target.view)
	{
		response->set_view(ViewName(target.view));
		ATL::CComBSTR config;
		if (SUCCEEDED(target.view->get_ReferencedConfiguration(&config)))
			response->set_view_configuration(ToUtf8(config));
	}
	response->set_checked_components(static_cast<int32_t>(GeometryComponents(assembly, false).size()));

	ShownForCheck shown(swApp, target.doc, target.active);
	ATL::CComPtr<IInterferenceDetectionMgr> mgr;
	HRESULT hr = assembly->get_InterferenceDetectionManager(&mgr);
	if (FAILED(hr))
		return ComError("IAssemblyDoc::InterferenceDetectionManager", hr);
	if (!mgr)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks has no interference detection for " + ToUtf8(title) +
			(target.view ? " here -- open the assembly itself (open_document) and check it there" : ""));

	VARIANT_BOOL dirtyBefore = VARIANT_FALSE;
	target.doc->GetSaveFlag(&dirtyBefore);
	std::vector<ATL::CComPtr<IDispatch>> found;
	{
		InterferenceSession session(mgr);
		session.Apply(*request);
		ATL::CComVariant interferences;
		hr = mgr->GetInterferences(&interferences);
		if (FAILED(hr))
			return ComError("IInterferenceDetectionMgr::GetInterferences", hr);
		found = comvariant::ToDispatches(interferences);

		ComponentList components(ids, math, response->mutable_components());
		for (const auto& disp : found)
		{
			ATL::CComQIPtr<IInterference> interference(disp.p);
			if (!interference)
				continue;
			if (response->interferences_size() >= kMaxInterferences)
			{
				response->set_truncated(true);
				break;
			}
			sld_mcp::v1::Interference* out = response->add_interferences();
			ATL::CComVariant list;
			if (SUCCEEDED(interference->get_Components(&list)))
				for (const auto& c : comvariant::ToDispatches(list))
					if (ATL::CComQIPtr<IComponent2> component(c.p); component)
					{
						out->add_component_persist_ids(components.Add(component));
						out->add_component_names(ComponentName(component));
					}
			double volume = 0.0;
			if (SUCCEEDED(interference->get_Volume(&volume)) && std::isfinite(volume))
				out->set_volume(volume);
			VARIANT_BOOL flag = VARIANT_FALSE;
			if (SUCCEEDED(interference->get_IsPossibleInterference(&flag)))
				out->set_possible(flag != VARIANT_FALSE);
			flag = VARIANT_FALSE;
			if (SUCCEEDED(interference->get_IsFastener(&flag)))
				out->set_fastener(flag != VARIANT_FALSE);
			flag = VARIANT_FALSE;
			if (SUCCEEDED(interference->get_Ignore(&flag)))
				out->set_ignored(flag != VARIANT_FALSE);
			ATL::CComPtr<IDispatch> bodyDisp;
			if (SUCCEEDED(interference->GetInterferenceBody(&bodyDisp)) && bodyDisp)
				if (ATL::CComQIPtr<IBody2> body(bodyDisp.p); body)
				{
					ATL::CComVariant box;
					if (SUCCEEDED(body->GetBodyBox(&box)))
						if (const std::vector<double> b = comvariant::ToDoubles(box); b.size() == 6)
							SetBox(out->mutable_box(), b);
				}
		}
	}
	VARIANT_BOOL dirtyAfter = VARIANT_FALSE;
	target.doc->GetSaveFlag(&dirtyAfter);
	response->set_marked_modified(!dirtyBefore && dirtyAfter);
	return grpc::Status::OK;
}

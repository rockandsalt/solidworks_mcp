// SolidWorksService.cpp : RPC implementations, each a thin wrapper over the
// SolidWorks API.
#include "pch.h"
#include "SolidWorksService.h"
#include <cmath>
#include <cstdio>
#include <string>

namespace
{
	// Joins the calling gRPC worker thread to the MTA for the duration of one
	// RPC. COM objects (including the proxy from the GIT) must be released
	// before this goes out of scope, so declare it first in each handler.
	class ComApartment
	{
	public:
		ComApartment() : m_hr(CoInitializeEx(nullptr, COINIT_MULTITHREADED)) {}
		~ComApartment()
		{
			if (SUCCEEDED(m_hr))
				CoUninitialize();
		}
		ComApartment(const ComApartment&) = delete;
		ComApartment& operator=(const ComApartment&) = delete;

		// RPC_E_CHANGED_MODE: the thread is already in an STA, which works too.
		bool Ok() const { return SUCCEEDED(m_hr) || m_hr == RPC_E_CHANGED_MODE; }

	private:
		HRESULT m_hr;
	};

	grpc::Status ComError(const char* what, HRESULT hr)
	{
		char message[128];
		std::snprintf(message, sizeof(message), "%s failed (HRESULT 0x%08lX)",
			what, static_cast<unsigned long>(hr));
		return grpc::Status(grpc::StatusCode::INTERNAL, message);
	}

	grpc::Status ComApartmentError()
	{
		return grpc::Status(grpc::StatusCode::INTERNAL, "CoInitializeEx failed on gRPC worker thread");
	}

	// protobuf strings are UTF-8; SolidWorks hands back UTF-16 BSTRs.
	std::string ToUtf8(const ATL::CComBSTR& s)
	{
		const int wideLen = static_cast<int>(s.Length());
		if (wideLen == 0)
			return {};
		const int len = WideCharToMultiByte(CP_UTF8, 0, s.m_str, wideLen, nullptr, 0, nullptr, nullptr);
		std::string out(static_cast<size_t>(len), '\0');
		WideCharToMultiByte(CP_UTF8, 0, s.m_str, wideLen, out.data(), len, nullptr, nullptr);
		return out;
	}

	ATL::CComBSTR FromUtf8(const std::string& s)
	{
		if (s.empty())
			return ATL::CComBSTR(L"");
		const int srcLen = static_cast<int>(s.size());
		const int len = MultiByteToWideChar(CP_UTF8, 0, s.data(), srcLen, nullptr, 0);
		ATL::CComBSTR out(len);
		MultiByteToWideChar(CP_UTF8, 0, s.data(), srcLen, out.m_str, len);
		return out;
	}

	// The active document, if it's a part with no sketch open for editing --
	// the state every model-building RPC needs to start from.
	grpc::Status GetEditablePart(ISldWorks* swApp, ATL::CComPtr<IModelDoc2>& doc)
	{
		HRESULT hr = swApp->get_IActiveDoc2(&doc);
		if (FAILED(hr))
			return ComError("ISldWorks::IActiveDoc2", hr);
		if (!doc)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"no document is open in SolidWorks -- create or open a part first");

		long docType = 0;
		hr = doc->GetType(&docType);
		if (FAILED(hr))
			return ComError("IModelDoc2::GetType", hr);
		if (docType != swDocPART)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"the active document isn't a part -- only parts are supported for now");

		// InsertSketch toggles, and features can't be added while a sketch is
		// being edited, so insist on a closed one.
		ATL::CComPtr<ISketch> openSketch;
		hr = doc->IGetActiveSketch2(&openSketch);
		if (FAILED(hr))
			return ComError("IModelDoc2::IGetActiveSketch2", hr);
		if (openSketch)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"a sketch is already open for editing in SolidWorks -- exit it first");
		return grpc::Status::OK;
	}

	// The last 2D sketch in the feature tree that no feature uses yet, i.e.
	// what a user means by "extrude this sketch". Sketches a feature has
	// absorbed have that feature as a child, so they're skipped.
	grpc::Status FindLatestUnusedSketch(IModelDoc2* doc, ATL::CComPtr<IFeature>& sketch)
	{
		ATL::CComPtr<IFeature> feature;
		HRESULT hr = doc->IFirstFeature(&feature);
		if (FAILED(hr))
			return ComError("IModelDoc2::IFirstFeature", hr);

		sketch.Release();
		while (feature)
		{
			ATL::CComBSTR typeName;
			hr = feature->GetTypeName2(&typeName);
			if (FAILED(hr))
				return ComError("IFeature::GetTypeName2", hr);
			if (typeName == L"ProfileFeature")
			{
				long children = 0;
				hr = feature->IGetChildCount(&children);
				if (FAILED(hr))
					return ComError("IFeature::IGetChildCount", hr);
				if (children == 0)
					sketch = feature;
			}

			ATL::CComPtr<IFeature> next;
			hr = feature->IGetNextFeature(&next);
			if (FAILED(hr))
				return ComError("IFeature::IGetNextFeature", hr);
			feature = next;
		}
		if (!sketch)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"the active part has no sketch that isn't already used by a feature -- "
				"create a sketch first");
		return grpc::Status::OK;
	}

	// Finds the `index`-th reference plane (0 = Front, 1 = Top, 2 = Right) by
	// walking the feature tree. Every part template starts with those three
	// as its first RefPlane features, so this works even when the planes are
	// renamed or SolidWorks runs in another language (SelectByID2 with
	// "Front Plane" would not).
	grpc::Status FindStandardPlane(IModelDoc2* doc, int index, ATL::CComPtr<IFeature>& plane)
	{
		ATL::CComPtr<IFeature> feature;
		HRESULT hr = doc->IFirstFeature(&feature);
		if (FAILED(hr))
			return ComError("IModelDoc2::IFirstFeature", hr);

		int planesSeen = 0;
		while (feature)
		{
			ATL::CComBSTR typeName;
			hr = feature->GetTypeName2(&typeName);
			if (FAILED(hr))
				return ComError("IFeature::GetTypeName2", hr);
			if (typeName == L"RefPlane" && planesSeen++ == index)
			{
				plane = feature;
				return grpc::Status::OK;
			}

			ATL::CComPtr<IFeature> next;
			hr = feature->IGetNextFeature(&next);
			if (FAILED(hr))
				return ComError("IFeature::IGetNextFeature", hr);
			feature = next;
		}
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"couldn't find the part's default Front/Top/Right planes");
	}
}

SolidWorksService::SolidWorksService(IGlobalInterfaceTable* git, DWORD swAppCookie)
	: m_git(git), m_swAppCookie(swAppCookie)
{
}

grpc::Status SolidWorksService::GetSwApp(ATL::CComPtr<ISldWorks>& swApp) const
{
	HRESULT hr = m_git->GetInterfaceFromGlobal(m_swAppCookie, __uuidof(ISldWorks),
		reinterpret_cast<void**>(&swApp));
	if (FAILED(hr))
		return grpc::Status(grpc::StatusCode::UNAVAILABLE, "lost connection to SolidWorks");
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::GetActiveDocument(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::GetActiveDocumentRequest* /*request*/,
	sld_mcp::v1::GetActiveDocumentResponse* response)
{
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	HRESULT hr = swApp->get_IActiveDoc2(&doc);
	if (FAILED(hr))
		return ComError("ISldWorks::IActiveDoc2", hr);
	if (!doc)
		return grpc::Status::OK; // nothing open: leave `document` unset

	ATL::CComBSTR title;
	hr = doc->GetTitle(&title);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetTitle", hr);

	response->mutable_document()->set_title(ToUtf8(title));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::NewPart(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::NewPartRequest* /*request*/,
	sld_mcp::v1::NewPartResponse* response)
{
	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	// Tools > Options > System Options > Default Templates > Parts
	ATL::CComBSTR templatePath;
	HRESULT hr = swApp->GetUserPreferenceStringValue(swDefaultTemplatePart, &templatePath);
	if (FAILED(hr))
		return ComError("ISldWorks::GetUserPreferenceStringValue", hr);
	if (templatePath.Length() == 0)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks has no default part template set -- set one under Tools > Options > "
			"System Options > Default Templates");

	// Paper size and dimensions only apply to drawings.
	ATL::CComPtr<IDispatch> created;
	hr = swApp->NewDocument(templatePath, 0, 0.0, 0.0, &created);
	if (FAILED(hr))
		return ComError("ISldWorks::NewDocument", hr);
	ATL::CComQIPtr<IModelDoc2> doc(created.p);
	if (!doc)
		return grpc::Status(grpc::StatusCode::INTERNAL,
			"SolidWorks couldn't create a part from the default template " + ToUtf8(templatePath));

	ATL::CComBSTR title;
	hr = doc->GetTitle(&title);
	if (FAILED(hr))
		return ComError("IModelDoc2::GetTitle", hr);

	response->mutable_document()->set_title(ToUtf8(title));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::CreateRectangleSketch(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::CreateRectangleSketchRequest* request,
	sld_mcp::v1::CreateRectangleSketchResponse* response)
{
	// Validate before touching SolidWorks.
	int planeIndex = 0;
	switch (request->plane())
	{
	case sld_mcp::v1::PLANE_FRONT: planeIndex = 0; break;
	case sld_mcp::v1::PLANE_TOP:   planeIndex = 1; break;
	case sld_mcp::v1::PLANE_RIGHT: planeIndex = 2; break;
	default:
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "plane must be FRONT, TOP or RIGHT");
	}
	const double cx = request->center_x();
	const double cy = request->center_y();
	const double halfWidth = request->width() / 2.0;
	const double halfHeight = request->height() / 2.0;
	if (!(std::isfinite(cx) && std::isfinite(cy)))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "center must be finite");
	if (!(std::isfinite(halfWidth) && halfWidth > 0.0 && std::isfinite(halfHeight) && halfHeight > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "width and height must be positive");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
		return status;

	ATL::CComPtr<IFeature> plane;
	if (grpc::Status status = FindStandardPlane(doc, planeIndex, plane); !status.ok())
		return status;

	ATL::CComPtr<ISketchManager> sketchManager;
	HRESULT hr = doc->get_SketchManager(&sketchManager);
	if (FAILED(hr))
		return ComError("IModelDoc2::SketchManager", hr);
	if (!sketchManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no sketch manager");

	hr = doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IModelDoc2::ClearSelection2", hr);
	VARIANT_BOOL selected = VARIANT_FALSE;
	hr = plane->Select2(VARIANT_FALSE, 0, &selected);
	if (FAILED(hr))
		return ComError("IFeature::Select2", hr);
	if (!selected)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select the sketch plane");

	// Opens a new sketch on the selected plane.
	hr = sketchManager->InsertSketch(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("ISketchManager::InsertSketch", hr);
	ATL::CComPtr<ISketch> sketch;
	hr = doc->IGetActiveSketch2(&sketch);
	if (FAILED(hr) || !sketch)
	{
		doc->ClearSelection2(VARIANT_TRUE);
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't open a sketch on the plane");
	}

	// The sketch is now open: everything below must fall through to the
	// InsertSketch call that closes it, so no early returns until then.

	// AddToDB skips snapping and inferencing, so the rectangle lands exactly
	// at the requested coordinates instead of snapping to nearby geometry.
	VARIANT_BOOL previousAddToDB = VARIANT_FALSE;
	sketchManager->get_AddToDB(&previousAddToDB);
	sketchManager->put_AddToDB(VARIANT_TRUE);

	// Center point, then one corner; sketch coordinates in meters. Returns
	// the created segments, or nothing if SolidWorks rejected the geometry.
	ATL::CComVariant segments;
	const HRESULT createHr = sketchManager->CreateCenterRectangle(
		cx, cy, 0.0, cx + halfWidth, cy + halfHeight, 0.0, &segments);
	const bool created = SUCCEEDED(createHr) && segments.vt != VT_EMPTY && segments.vt != VT_NULL;

	sketchManager->put_AddToDB(previousAddToDB);

	// An ISketch is also its IFeature in the tree; read the name while it's
	// still the active sketch.
	ATL::CComQIPtr<IFeature> sketchFeature(sketch.p);
	ATL::CComBSTR sketchName;
	if (sketchFeature)
		sketchFeature->get_Name(&sketchName);

	// Close the sketch (and rebuild).
	const HRESULT closeHr = sketchManager->InsertSketch(VARIANT_TRUE);
	doc->ClearSelection2(VARIANT_TRUE);

	if (!created)
	{
		// Don't leave an empty sketch behind.
		VARIANT_BOOL reselected = VARIANT_FALSE;
		if (SUCCEEDED(closeHr) && sketchFeature &&
			SUCCEEDED(sketchFeature->Select2(VARIANT_FALSE, 0, &reselected)) && reselected)
		{
			doc->EditDelete();
			doc->ClearSelection2(VARIANT_TRUE);
		}
		if (FAILED(createHr))
			return ComError("ISketchManager::CreateCenterRectangle", createHr);
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks didn't create the rectangle");
	}
	if (FAILED(closeHr))
		return ComError("ISketchManager::InsertSketch (closing the sketch)", closeHr);

	response->set_sketch_name(ToUtf8(sketchName));
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::ExtrudeSketch(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::ExtrudeSketchRequest* request,
	sld_mcp::v1::ExtrudeSketchResponse* response)
{
	// Validate before touching SolidWorks.
	const double depth = request->depth();
	if (!(std::isfinite(depth) && depth > 0.0))
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "depth must be positive");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;

	ATL::CComPtr<IModelDoc2> doc;
	if (grpc::Status status = GetEditablePart(swApp, doc); !status.ok())
		return status;

	// Resolve the sketch: by name if given, else the latest unused one.
	ATL::CComPtr<IFeature> sketch;
	HRESULT hr = S_OK;
	if (request->sketch_name().empty())
	{
		if (grpc::Status status = FindLatestUnusedSketch(doc, sketch); !status.ok())
			return status;
	}
	else
	{
		ATL::CComQIPtr<IPartDoc> part(doc.p);
		if (!part)
			return grpc::Status(grpc::StatusCode::INTERNAL, "active part has no IPartDoc interface");
		hr = part->IFeatureByName(FromUtf8(request->sketch_name()), &sketch);
		if (FAILED(hr))
			return ComError("IPartDoc::IFeatureByName", hr);
		if (!sketch)
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"the active part has no feature named \"" + request->sketch_name() + "\"");

		ATL::CComBSTR typeName;
		hr = sketch->GetTypeName2(&typeName);
		if (FAILED(hr))
			return ComError("IFeature::GetTypeName2", hr);
		if (typeName != L"ProfileFeature")
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"\"" + request->sketch_name() + "\" isn't a 2D sketch");
	}

	ATL::CComBSTR sketchName;
	hr = sketch->get_Name(&sketchName);
	if (FAILED(hr))
		return ComError("IFeature::Name", hr);

	ATL::CComPtr<IFeatureManager> featureManager;
	hr = doc->get_FeatureManager(&featureManager);
	if (FAILED(hr))
		return ComError("IModelDoc2::FeatureManager", hr);
	if (!featureManager)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks returned no feature manager");

	// FeatureExtrusion3 extrudes whatever profile is selected.
	hr = doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IModelDoc2::ClearSelection2", hr);
	VARIANT_BOOL selected = VARIANT_FALSE;
	hr = sketch->Select2(VARIANT_FALSE, 0, &selected);
	if (FAILED(hr))
		return ComError("IFeature::Select2", hr);
	if (!selected)
		return grpc::Status(grpc::StatusCode::INTERNAL, "SolidWorks wouldn't select the sketch");

	// Mid-plane takes the total depth in D1 and splits it across both sides;
	// it has no direction, so `reverse_direction` doesn't apply.
	const bool midPlane = request->mid_plane();
	const VARIANT_BOOL reverse = (!midPlane && request->reverse_direction()) ? VARIANT_TRUE : VARIANT_FALSE;
	const long endCondition = midPlane ? swEndCondMidPlane : swEndCondBlind;

	ATL::CComPtr<IFeature> extrusion;
	hr = featureManager->FeatureExtrusion3(
		VARIANT_TRUE,       // Sd: one direction only
		VARIANT_FALSE,      // Flip: side to cut (cuts only)
		reverse,            // Dir: reverse direction 1
		endCondition,       // T1
		swEndCondBlind,     // T2 (unused, single direction)
		depth,              // D1
		0.0,                // D2
		VARIANT_FALSE,      // Dchk1: no draft
		VARIANT_FALSE,      // Dchk2
		VARIANT_FALSE,      // Ddir1
		VARIANT_FALSE,      // Ddir2
		0.0,                // Dang1
		0.0,                // Dang2
		VARIANT_FALSE,      // OffsetReverse1
		VARIANT_FALSE,      // OffsetReverse2
		VARIANT_FALSE,      // TranslateSurface1
		VARIANT_FALSE,      // TranslateSurface2
		VARIANT_TRUE,       // Merge with existing bodies
		VARIANT_TRUE,       // UseFeatScope
		VARIANT_TRUE,       // UseAutoSelect: all bodies in scope
		swStartSketchPlane, // T0: start at the sketch plane
		0.0,                // StartOffset
		VARIANT_FALSE,      // FlipStartOffset
		&extrusion);
	doc->ClearSelection2(VARIANT_TRUE);
	if (FAILED(hr))
		return ComError("IFeatureManager::FeatureExtrusion3", hr);
	if (!extrusion)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"SolidWorks couldn't extrude " + ToUtf8(sketchName) +
			" -- the sketch must contain a closed, non-self-intersecting profile");

	ATL::CComBSTR featureName;
	hr = extrusion->get_Name(&featureName);
	if (FAILED(hr))
		return ComError("IFeature::Name", hr);

	response->set_feature_name(ToUtf8(featureName));
	response->set_sketch_name(ToUtf8(sketchName));
	return grpc::Status::OK;
}

// Undo.cpp : Undo (taking back the active document's last steps).
#include "pch.h"
#include "Common.h"
#include "AssemblyHelpers.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"
#include "SeedHelpers.h"

using namespace sld_mcp::service;

namespace
{
	constexpr int kMaxSteps = 100;

	// More than a sketch tool call leaves.
	constexpr int kMaxSketchSteps = 1000;

	// EditUndo2 by name. Through the vtable it can't get past a sketch: back
	// over its closing opens it, and the next step closes it again with all it
	// had, for ever; by name, the next steps take its contents and then the
	// sketch (seen 2026-11-01, both also through the interop assembly).
	HRESULT UndoSteps(IModelDoc2* doc, long steps)
	{
		ATL::CComQIPtr<IDispatch> dispatch(doc);
		if (!dispatch)
			return E_NOINTERFACE;
		ATL::CComVariant count(steps);
		return dispatch.Invoke1(L"EditUndo2", &count);
	}

	// The features of `from` that `other` has none of the name of.
	void AddMissing(const std::vector<TreeFeature>& from, const std::vector<TreeFeature>& other,
		google::protobuf::RepeatedPtrField<sld_mcp::v1::FeatureRef>* out)
	{
		for (const TreeFeature& f : from)
			if (std::none_of(other.begin(), other.end(), [&](const TreeFeature& o) { return o.name == f.name; }))
				SetFeatureRef(out->Add(), f.name, f.type);
	}
}

grpc::Status SolidWorksService::Undo(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::UndoRequest* request,
	sld_mcp::v1::UndoResponse* response)
{
	if (request->steps() < 1 || request->steps() > kMaxSteps)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
			"steps must be between 1 and " + std::to_string(kMaxSteps));

	std::lock_guard<std::mutex> lock(m_editMutex);
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
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "no document is open in SolidWorks");
	// A drawing's sheet always counts as its open sketch (seen 2026-11-01).
	long docType = swDocNONE;
	doc->GetType(&docType);
	const bool hasSketches = docType != swDocDRAWING;
	if (hasSketches)
		if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
			return status;

	ATL::CComBSTR title;
	if (SUCCEEDED(doc->GetTitle(&title)))
		response->set_document(ToUtf8(title));

	PreferenceToggles toggles(swApp);
	toggles.Set(swShowErrorsEveryRebuild, false);

	// EditUndo2 returns nothing, and takes more steps than there are without
	// complaint (seen 2026-11-01), so what it did is measured.
	const std::vector<TreeFeature> before = FeatureTree(doc);
	response->set_volume_before(SolidVolume(doc));
	hr = UndoSteps(doc, request->steps());
	if (FAILED(hr))
		return ComError("IModelDoc2::EditUndo2", hr);
	// A step back over a sketch's closing leaves it open for editing (seen
	// 2026-11-01), where other tools refuse to work and none closes it, so a
	// sketch call is taken back whole.
	int taken = request->steps();
	const auto sketchOpen = [&] { return hasSketches && !CheckNoOpenSketch(doc).ok(); };
	for (; sketchOpen() && taken < request->steps() + kMaxSketchSteps; ++taken)
		if (FAILED(hr = UndoSteps(doc, 1)))
			return ComError("IModelDoc2::EditUndo2", hr);
	if (sketchOpen())
		return grpc::Status(grpc::StatusCode::INTERNAL, "undoing left a sketch open for editing");
	response->set_steps_taken(taken);
	const std::vector<TreeFeature> after = FeatureTree(doc);
	response->set_volume_after(SolidVolume(doc));
	AddMissing(before, after, response->mutable_removed());
	AddMissing(after, before, response->mutable_restored());
	doc->GraphicsRedraw2();
	return grpc::Status::OK;
}

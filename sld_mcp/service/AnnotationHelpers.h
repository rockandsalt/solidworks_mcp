// AnnotationHelpers.h : Annotations (PMI / MBD): collecting and describing them.
//
// Shared by the RPC implementations in this directory.
#pragma once

#include "Common.h"
#include "AssemblyHelpers.h"

namespace sld_mcp::service
{
	constexpr size_t kMaxAnnotations = 5000;

	constexpr int kMaxAttachedPerAnnotation = 200;

	constexpr long kMaxAnnotationTexts = 50;

	constexpr short kMaxGtolFrames = 10;

	// Late-bound call of a property or argument-less method by name. Used for
	// DimXpert objects (IDimXpertFeature, ...), whose interfaces live in
	// swdimxpert.tlb, which the add-in doesn't #import.
	bool InvokeByName(IDispatch* obj, const wchar_t* name, ATL::CComVariant& result);

	// Late-bound call with arguments (in their order), or with `put` a
	// property assignment (one argument). `result` may be null.
	bool CallByName(IDispatch* obj, const wchar_t* name, std::vector<ATL::CComVariant> args,
		ATL::CComVariant* result, bool put = false);

	// The active configuration's IDimXpertPart (late-bound); null if there is
	// none (a part without a DimXpert scheme, unless `create`) or the
	// document isn't a part.
	ATL::CComPtr<IDispatch> DimXpertPartOf(IModelDoc2* doc, IModelDocExtension* ext, bool create);

	// IDimXpertPart::GetAnnotations.
	std::vector<ATL::CComPtr<IDispatch>> DimXpertAnnotations(IDispatch* dimXpertPart);

	// A DimXpert dimension under a general or block tolerance: the limits
	// DimXpert works out (`dimXpertAnnotation` is its IDimXpertAnnotation).
	void DescribeDimXpertLimits(IDispatch* dimXpertAnnotation, sld_mcp::v1::AnnotationDimension* out);

	// Every annotation of the document, once each. IModelDocExtension::
	// GetAnnotations should have them all; the annotation views' lists and
	// the GetFirstAnnotation2 chain are merged in too in case it leaves some
	// out (it isn't documented whether DimXpert's or hidden ones are in it).
	std::vector<ATL::CComPtr<IAnnotation>> AllAnnotations(IModelDoc2* doc, IModelDocExtension* ext,
		bool& truncated);

	void AddText(const ATL::CComBSTR& text, sld_mcp::v1::AnnotationInfo* info);

	// The pieces of text of the annotation types that list them by index
	// (datum targets, feature control frames, surface finish symbols, ...).
	template <typename T>
	void AddIndexedTexts(T* annotation, sld_mcp::v1::AnnotationInfo* info)
	{
		long count = 0;
		if (FAILED(annotation->GetTextCount(&count)))
			return;
		for (long i = 0; i < count && i < kMaxAnnotationTexts; ++i)
		{
			ATL::CComBSTR text;
			if (SUCCEEDED(annotation->GetTextAtIndex(i, &text)))
				AddText(text, info);
		}
	}

	// Classifies an entity an annotation is attached to and references it.
	void DescribeAttached(IDispatch* obj, PersistIds& ids, ComponentList* components,
		sld_mcp::v1::AttachedEntity* out);

	// What the IDimension itself knows: reference, tolerance, owning feature.
	void DescribeModelDimension(IDimension* dim, sld_mcp::v1::AnnotationDimension* out);

	void DescribeAnnotationDimension(IDisplayDimension* display, sld_mcp::v1::AnnotationDimension* out);

	void DescribeGtol(IGtol* gtol, sld_mcp::v1::AnnotationGtol* out);

	// A 2022-format frame's diameter zones, material conditions and
	// composite flag, from its rows' XML (after DescribeGtol read the rest).
	void DescribeFormat2Frames(IGtol* gtol, sld_mcp::v1::AnnotationGtol* out);

	constexpr long kMaxAnnotationLeaders = 50;

	// What a drawing annotation belongs to (UNSPECIFIED outside drawings),
	// and with `ownerName` that view's, sheet's or sheet format's name.
	sld_mcp::v1::AnnotationOwner AnnotationOwnerOf(IAnnotation* annotation, std::string* ownerName);

	// Leader style and each leader's points and arrow.
	void DescribeLeaders(IAnnotation* annotation, sld_mcp::v1::AnnotationInfo* info);

	// A note's look: its first text format, justification, angle, border,
	// extent.
	void DescribeNote(INote* note, IAnnotation* annotation, sld_mcp::v1::NoteInfo* out);

	// A BOM balloon's texts and component (nothing for other notes).
	void DescribeBalloon(INote* note, IAnnotation* annotation, sld_mcp::v1::AnnotationInfo* info);

	void DescribeAnnotation(IAnnotation* annotation, PersistIds& ids, ComponentList* components,
		sld_mcp::v1::AnnotationInfo* info);

	// Selects the annotations and deletes them; false if none could be
	// selected or SolidWorks refused.
	bool DeleteAnnotations(IModelDoc2* doc, IModelDocExtension* ext,
		const std::vector<ATL::CComPtr<IAnnotation>>& annotations);

	// A feature control frame, datum feature, surface finish or weld symbol
	// to add: what can be checked without SolidWorks.
	grpc::Status ValidateGdtSymbol(const sld_mcp::v1::GdtSymbol& symbol);

	// The letters of the datum feature symbols among `annotations`.
	std::vector<std::string> DatumLabels(const std::vector<ATL::CComPtr<IAnnotation>>& annotations);

	// INVALID_ARGUMENT if `symbol` is a datum symbol whose letter one of
	// `taken` already has.
	grpc::Status CheckDatumLabelFree(const sld_mcp::v1::GdtSymbol& symbol, const std::vector<std::string>& taken);

	// Inserts the symbol for what's selected -- IModelDoc2::InsertGtol /
	// InsertDatumTag2 give it a leader to a selected entity, or put it on a
	// selected dimension; a surface finish or weld symbol gets a leader the
	// same way -- in the active (drawing) view, and gives it the frames /
	// letter / texts `symbol` asks for, each read back. `at`: where a surface
	// finish symbol's point goes. On failure the new annotation is deleted
	// again.
	grpc::Status InsertGdtSymbol(IModelDoc2* doc, IModelDocExtension* ext, const sld_mcp::v1::GdtSymbol& symbol,
		const Eigen::Vector3d& at, ATL::CComPtr<IAnnotation>& out);

	// Whether a feature control frame or datum symbol is attached to an
	// entity (a leader to it) or a drawing dimension (it sits on it):
	// IGtol::IsAttached, or SolidWorks reports what it's attached to.
	bool GdtAttached(IAnnotation* annotation);
}

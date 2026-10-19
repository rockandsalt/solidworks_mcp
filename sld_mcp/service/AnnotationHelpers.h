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

	void DescribeAnnotation(IAnnotation* annotation, PersistIds& ids, ComponentList* components,
		sld_mcp::v1::AnnotationInfo* info);
}

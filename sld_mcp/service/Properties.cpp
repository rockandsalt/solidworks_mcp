// Properties.cpp : Document properties: CustomProperties (custom properties, a configuration's BOM part
// number) and PartMaterial (a part's physical material), of the active document, an open one, or the
// model a drawing sheet takes its properties from (see docs/notes/drawings.md, "Document properties").
#include "pch.h"
#include "Common.h"
#include "DrawingHelpers.h"

using namespace sld_mcp::service;

namespace
{
	constexpr int kMaxProperties = 500;

	// The model whose properties a sheet's $PRPSHEET links show: the
	// model of the sheet's custom property view ("Default": its first
	// view). A new sheet's property view is "Same as sheet specified in
	// Document Properties" (seen 2026-10-22; localized): the sheet whose
	// number swDrawingSheetCustomPropSheetNo holds (1 = the first) decides.
	// `configuration` = the view's referenced configuration.
	grpc::Status SheetPropertyModel(const DrawingSheet& sheet, const std::vector<DrawingSheet>& sheets,
		IModelDocExtension* drawingExt, ATL::CComPtr<IModelDoc2>& model, std::string& configuration, int depth = 0)
	{
		ATL::CComBSTR viewName;
		if (sheet.sheet)
			sheet.sheet->get_CustomPropertyView(&viewName);
		ATL::CComPtr<IView> view;
		if (viewName.Length() == 0 || _wcsicmp(viewName, L"Default") == 0)
		{
			if (sheet.views.empty())
				return grpc::Status(grpc::StatusCode::NOT_FOUND, sheet.name +
					" has no views, so no model to take properties from");
			view = sheet.views[0];
		}
		else
		{
			std::string sheetName;
			if (!FindDrawingView(sheets, ToUtf8(viewName), view, sheetName).ok())
			{
				long number = 1;
				if (drawingExt)
					drawingExt->GetUserPreferenceInteger(swDrawingSheetCustomPropSheetNo, swDetailingNoOptionSpecified, &number);
				const DrawingSheet& from = sheets[std::clamp<long>(number, 1, static_cast<long>(sheets.size())) - 1];
				if (&from == &sheet || depth > 0)
				{
					if (sheet.views.empty())
						return grpc::Status(grpc::StatusCode::NOT_FOUND, sheet.name +
							" has no views, so no model to take properties from");
					view = sheet.views[0];
				}
				else
					return SheetPropertyModel(from, sheets, drawingExt, model, configuration, depth + 1);
			}
		}
		if (FAILED(view->get_ReferencedDocument(&model)) || !model)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, ViewName(view) +
				"'s model isn't loaded (resolve the view first)");
		ATL::CComBSTR text;
		if (SUCCEEDED(view->get_ReferencedConfiguration(&text)))
			configuration = ToUtf8(text);
		return grpc::Status::OK;
	}

	// The document a properties request is about: `document` (an open one
	// by title or full path), the model of sheet `sheetName` of the active
	// drawing ("*" = its active sheet), or else the active document.
	// `viewConfiguration` = the sheet view's configuration (empty otherwise).
	grpc::Status PropertyDocument(ISldWorks* swApp, const std::string& document, const std::string& sheetName,
		ATL::CComPtr<IModelDoc2>& doc, std::string& viewConfiguration)
	{
		if (!document.empty())
		{
			if (!sheetName.empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give a document or a sheet, not both");
			ATL::CComPtr<IDispatch> found;
			if (FAILED(swApp->GetOpenDocumentByName(FromUtf8(document), &found)) || !found ||
				!(doc = ATL::CComQIPtr<IModelDoc2>(found.p)))
				return grpc::Status(grpc::StatusCode::NOT_FOUND, "no open document is called \"" + document +
					"\" (open_document opens a file; a drawing's or assembly's models are open while it is)");
			return grpc::Status::OK;
		}
		ATL::CComPtr<IDispatch> active;
		if (FAILED(swApp->get_ActiveDoc(&active)) || !active || !(doc = ATL::CComQIPtr<IModelDoc2>(active.p)))
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "no document is open in SolidWorks");
		if (sheetName.empty())
			return grpc::Status::OK;
		ATL::CComQIPtr<IDrawingDoc> drawing(doc.p);
		if (!drawing)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "a sheet's model needs a drawing as the active document");
		const std::vector<DrawingSheet> sheets = DrawingSheets(drawing);
		const std::string wanted = sheetName == "*" ? ActiveSheetName(drawing) : sheetName;
		const DrawingSheet* sheet = FindSheet(sheets, wanted);
		if (!sheet)
			return grpc::Status(grpc::StatusCode::NOT_FOUND,
				"the drawing has no sheet named \"" + wanted + "\" (its sheets: " + SheetList(sheets) + ")");
		ATL::CComPtr<IModelDoc2> model;
		ATL::CComPtr<IModelDocExtension> drawingExt;
		doc->get_Extension(&drawingExt);
		if (grpc::Status status = SheetPropertyModel(*sheet, sheets, drawingExt, model, viewConfiguration); !status.ok())
			return status;
		doc = model;
		return grpc::Status::OK;
	}

	// A configuration by name; empty = the active one. `name` gets its name.
	grpc::Status ConfigurationOf(IModelDoc2* doc, std::string& name, ATL::CComPtr<IConfiguration>& config)
	{
		ATL::CComPtr<IDispatch> disp;
		if (name.empty())
			doc->GetActiveConfiguration(&disp);
		else
			doc->GetConfigurationByName(FromUtf8(name), &disp);
		if (!disp || !(config = ATL::CComQIPtr<IConfiguration>(disp.p)))
			return grpc::Status(grpc::StatusCode::NOT_FOUND, name.empty() ? "the document has no active configuration"
				: "no configuration named \"" + name + "\"");
		ATL::CComBSTR text;
		if (SUCCEEDED(config->get_Name(&text)))
			name = ToUtf8(text);
		return grpc::Status::OK;
	}

	void SetDocument(IModelDoc2* doc, sld_mcp::v1::Document* out)
	{
		ATL::CComBSTR title, path;
		doc->GetTitle(&title);
		doc->GetPathName(&path);
		long type = swDocNONE;
		doc->GetType(&type);
		out->set_title(ToUtf8(title));
		out->set_path(ToUtf8(path));
		out->set_type(ToDocumentType(type));
	}

	// A property's value as typed and as evaluated; false if there's none.
	bool GetProperty(ICustomPropertyManager* properties, BSTR name, sld_mcp::v1::CustomProperty* out)
	{
		ATL::CComBSTR value, resolved;
		VARIANT_BOOL wasResolved = VARIANT_FALSE, linked = VARIANT_FALSE;
		long status = swCustomInfoGetResult_NotPresent;
		if (FAILED(properties->Get6(name, VARIANT_FALSE, &value, &resolved, &wasResolved, &linked, &status)) ||
			status == swCustomInfoGetResult_NotPresent)
			return false;
		out->set_name(ToUtf8(name));
		out->set_value(ToUtf8(value));
		out->set_resolved(ToUtf8(resolved));
		long t = 0;
		if (SUCCEEDED(properties->GetType2(name, &t)))
			out->set_type(static_cast<int32_t>(t));
		return true;
	}

	// A value that links one SolidWorks works out: a quoted reference such
	// as "SW-Material@part.SLDPRT" or "D1@Sketch1@part.SLDPRT".
	bool IsLink(const sld_mcp::v1::CustomProperty& p)
	{
		return p.value() != p.resolved() && p.value().find('"') != std::string::npos &&
			p.value().find('@') != std::string::npos;
	}

	// Notes linking the properties ($PRP / $PRPSHEET in a title block)
	// show the old values until a rebuild of the drawing (seen 2026-10-22).
	void RebuildAfterChange(ISldWorks* swApp, IModelDoc2* doc)
	{
		VARIANT_BOOL rebuilt = VARIANT_FALSE;
		doc->EditRebuild3(&rebuilt);
		ATL::CComPtr<IDispatch> activeDisp;
		if (SUCCEEDED(swApp->get_ActiveDoc(&activeDisp)) && activeDisp)
			if (ATL::CComQIPtr<IModelDoc2> activeDoc(activeDisp.p); activeDoc && !SameObject(activeDoc, doc))
				activeDoc->EditRebuild3(&rebuilt);
	}

	// The configuration's BOM part number to user specified `text`. Setting
	// BOMPartNoSource to swBOMPartNumber_UserSpecified is ignored (seen
	// 2026-10-22); AlternateName + UseAlternateNameInBOM make it that.
	grpc::Status SetBomPartNumber(IConfiguration* config, const std::string& text)
	{
		const ATL::CComBSTR wanted = FromUtf8(text);
		config->put_AlternateName(wanted);
		config->put_UseAlternateNameInBOM(VARIANT_TRUE);
		long source = 0;
		ATL::CComBSTR now;
		config->get_BOMPartNoSource(&source);
		config->get_AlternateName(&now);
		if (source != swBOMPartNumber_UserSpecified || ToUtf8(now) != text)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "SolidWorks didn't take BOM part number \"" + text +
				"\" (it shows " + (source == swBOMPartNumber_UserSpecified ? "\"" + ToUtf8(now) + "\"" :
				"source " + std::to_string(source)) + ")");
		return grpc::Status::OK;
	}

	// Where SolidWorks looks for material databases: Tools > Options > File
	// Locations (empty on a fresh install, seen 2026-10-22), then the
	// install's own folder.
	std::vector<std::string> MaterialDatabaseFolders(ISldWorks* swApp)
	{
		std::vector<std::string> out;
		ATL::CComBSTR locations;
		if (SUCCEEDED(swApp->GetUserPreferenceStringValue(swFileLocationsMaterialDatabases, &locations)))
		{
			std::string all = ToUtf8(locations);
			size_t start = 0;
			while (start <= all.size())
			{
				const size_t end = std::min(all.find(';', start), all.size());
				if (end > start)
					out.push_back(all.substr(start, end - start));
				start = end + 1;
			}
		}
		ATL::CComBSTR exe, language;
		if (SUCCEEDED(swApp->GetExecutablePath(&exe)) && exe.Length() > 0 &&
			SUCCEEDED(swApp->GetCurrentLanguage(&language)) && language.Length() > 0)
			out.push_back(ToUtf8(exe) + "\\lang\\" + ToUtf8(language) + "\\sldmaterials");
		return out;
	}

	std::string MaterialName(IPartDoc* part, const std::string& configuration, std::string& database)
	{
		ATL::CComBSTR name, db;
		part->GetMaterialPropertyName2(FromUtf8(configuration), &db, &name);
		database = ToUtf8(db);
		return ToUtf8(name);
	}
}

grpc::Status SolidWorksService::CustomProperties(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::CustomPropertiesRequest* request,
	sld_mcp::v1::CustomPropertiesResponse* response)
{
	if (request->set_size() + request->remove_size() > kMaxProperties)
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "at most " + std::to_string(kMaxProperties) + " properties per call");
	for (const auto& p : request->set())
		if (p.name().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a property needs a name");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;
	ATL::CComPtr<IModelDoc2> doc;
	std::string viewConfiguration;
	if (grpc::Status status = PropertyDocument(swApp, request->document(), request->sheet_name(), doc, viewConfiguration);
		!status.ok())
		return status;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	ATL::CComPtr<ICustomPropertyManager> properties;
	if (FAILED(ext->get_CustomPropertyManager(FromUtf8(request->configuration()), &properties)) || !properties)
		return grpc::Status(grpc::StatusCode::NOT_FOUND, "no configuration named \"" + request->configuration() + "\"");
	long docType = swDocNONE;
	doc->GetType(&docType);
	const bool hasConfigurations = docType == swDocPART || docType == swDocASSEMBLY;
	ATL::CComPtr<IConfiguration> config;
	std::string bomConfiguration = request->configuration().empty() ? viewConfiguration : request->configuration();
	if (hasConfigurations)
		if (grpc::Status status = ConfigurationOf(doc, bomConfiguration, config); !status.ok())
			return status;
	if (!request->bom_part_number().empty() && !config)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
			"a BOM part number belongs to a part's or assembly's configuration, not a drawing");

	// Everything is checked before anything changes.
	auto refuseLink = [&](const std::string& name, const char* what) -> grpc::Status {
		sld_mcp::v1::CustomProperty current;
		if (request->replace_links() || !GetProperty(properties, FromUtf8(name), &current) || !IsLink(current))
			return grpc::Status::OK;
		std::string message = std::string("won't ") + what + " property \"" + name + "\": it links " + current.value() +
			" (now \"" + current.resolved() + "\"), which SolidWorks keeps up to date; replace_links overwrites it";
		if (current.value().find("SW-Material") != std::string::npos)
			message += ". It shows the part's physical material: part_material changes that, and this property follows";
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, message);
	};
	for (const auto& name : request->remove())
		if (grpc::Status status = refuseLink(name, "remove"); !status.ok())
			return status;
	for (const auto& p : request->set())
		if (grpc::Status status = refuseLink(p.name(), "replace"); !status.ok())
			return status;

	for (const auto& name : request->remove())
	{
		long result = 0;
		properties->Delete2(FromUtf8(name), &result);
	}
	for (const auto& p : request->set())
	{
		long result = 0;
		HRESULT hr = properties->Add3(FromUtf8(p.name()), swCustomInfoText, FromUtf8(p.value()),
			swCustomPropertyReplaceValue, &result);
		if (FAILED(hr) || result != swCustomInfoAddResult_AddedOrChanged)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "SolidWorks didn't set property \"" + p.name() +
				"\" (result " + std::to_string(result) + "; an existing property of another type?)");
	}
	if (!request->bom_part_number().empty())
		if (grpc::Status status = SetBomPartNumber(config, request->bom_part_number()); !status.ok())
			return status;
	if (request->set_size() + request->remove_size() > 0 || !request->bom_part_number().empty())
		RebuildAfterChange(swApp, doc);

	SetDocument(doc, response->mutable_document());
	response->set_configuration(request->configuration());
	ATL::CComVariant names;
	if (SUCCEEDED(properties->GetNames(&names)))
		for (const auto& name : comvariant::ToBstrs(names))
			GetProperty(properties, name, response->add_properties());
	if (config)
	{
		response->set_bom_configuration(bomConfiguration);
		long source = 0;
		config->get_BOMPartNoSource(&source);
		response->set_bom_part_number_source(static_cast<int32_t>(source));
		ATL::CComBSTR alternate;
		config->get_AlternateName(&alternate);
		response->set_bom_part_number(ToUtf8(alternate));
	}
	return grpc::Status::OK;
}

grpc::Status SolidWorksService::PartMaterial(grpc::ServerContext* /*context*/,
	const sld_mcp::v1::PartMaterialRequest* request,
	sld_mcp::v1::PartMaterialResponse* response)
{
	if (request->remove() && !request->material().empty())
		return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "give a material or remove, not both");

	std::lock_guard<std::mutex> lock(m_editMutex);
	ComApartment com;
	if (!com.Ok())
		return ComApartmentError();

	ATL::CComPtr<ISldWorks> swApp;
	if (grpc::Status status = GetSwApp(swApp); !status.ok())
		return status;
	ATL::CComPtr<IModelDoc2> doc;
	std::string configuration;
	if (grpc::Status status = PropertyDocument(swApp, request->document(), request->sheet_name(), doc, configuration);
		!status.ok())
		return status;
	SetDocument(doc, response->mutable_document());
	for (const auto& folder : MaterialDatabaseFolders(swApp))
		response->add_database_folders(folder);
	ATL::CComQIPtr<IPartDoc> part(doc.p);
	if (!part)
		return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, response->document().title() +
			" isn't a part: a physical material is assigned to each part (give the part as document)");
	if (!request->configuration().empty())
		configuration = request->configuration();
	ATL::CComPtr<IConfiguration> config;
	if (grpc::Status status = ConfigurationOf(doc, configuration, config); !status.ok())
		return status;

	if (request->remove() || !request->material().empty())
	{
		// An unknown name is ignored without an error (seen 2026-10-22),
		// so the result is read back.
		HRESULT hr = part->SetMaterialPropertyName2(FromUtf8(configuration), FromUtf8(request->database()),
			FromUtf8(request->material()));
		if (FAILED(hr))
			return ComError("IPartDoc::SetMaterialPropertyName2", hr);
		std::string database;
		const std::string now = MaterialName(part, configuration, database);
		if (now != request->material())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, request->remove()
				? "SolidWorks didn't remove the material (still \"" + now + "\")"
				: "SolidWorks didn't take material \"" + request->material() + "\"" +
					(request->database().empty() ? "" : " of database \"" + request->database() + "\"") +
					" (names are exact, as in the database; the part still has " +
					(now.empty() ? "no material" : "\"" + now + "\"") + ")");
		RebuildAfterChange(swApp, doc);
	}

	std::string database;
	response->set_configuration(configuration);
	response->set_material(MaterialName(part, configuration, database));
	response->set_database(database);

	// Mass properties are the active configuration's.
	std::string active;
	ATL::CComPtr<IConfiguration> activeConfig;
	ATL::CComPtr<IModelDocExtension> ext;
	if (grpc::Status status = GetExtension(doc, ext); !status.ok())
		return status;
	if (ConfigurationOf(doc, active, activeConfig).ok() && active == configuration)
	{
		ATL::CComPtr<IDispatch> disp;
		ATL::CComQIPtr<IMassProperty2> mass;
		double kg = 0, density = 0;
		if (SUCCEEDED(ext->CreateMassProperty2(&disp)) && disp && (mass = disp.p) &&
			SUCCEEDED(mass->get_Mass(&kg)) && SUCCEEDED(mass->get_Density(&density)))
		{
			response->set_has_mass(true);
			response->set_mass(kg);
			response->set_density(density);
		}
	}

	ATL::CComPtr<ICustomPropertyManager> fileProperties, configProperties;
	const ATL::CComBSTR materialName(L"Material");
	if (SUCCEEDED(ext->get_CustomPropertyManager(ATL::CComBSTR(L""), &fileProperties)) && fileProperties)
	{
		sld_mcp::v1::CustomProperty p;
		if (GetProperty(fileProperties, materialName, &p))
			*response->mutable_material_property() = p;
	}
	if (SUCCEEDED(ext->get_CustomPropertyManager(FromUtf8(configuration), &configProperties)) && configProperties)
	{
		sld_mcp::v1::CustomProperty p;
		if (GetProperty(configProperties, materialName, &p))
			*response->mutable_configuration_material_property() = p;
	}
	return grpc::Status::OK;
}

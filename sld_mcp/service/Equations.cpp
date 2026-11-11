// Equations.cpp : Equations (equations and global variables: listing, setting, deleting)
// and DesignTable (see docs/notes/equations.md).
#include "pch.h"
#include "Common.h"
#include "ConfigurationHelpers.h"
#include "DimensionHelpers.h"
#include "FeatureTreeHelpers.h"

using namespace sld_mcp::service;

namespace
{
	// More than a table anyone types; SolidWorks stops reading at the first
	// empty header cell or configuration name anyway.
	constexpr long kMaxColumns = 200;
	constexpr long kMaxRows = 1000;

	grpc::Status GetEquationMgr(IModelDoc2* doc, ATL::CComPtr<IEquationMgr>& equations)
	{
		HRESULT hr = doc->GetEquationMgr(&equations);
		if (FAILED(hr) || !equations)
			return ComError("IModelDoc2::GetEquationMgr", hr);
		return grpc::Status::OK;
	}

	// The index of the equation whose left side is `name`, or -1.
	long FindEquation(const std::vector<std::string>& equations, const std::string& name)
	{
		for (size_t i = 0; i < equations.size(); ++i)
			if (_stricmp(EquationName(equations[i]).c_str(), name.c_str()) == 0)
				return static_cast<long>(i);
		return -1;
	}

	// Puts the equations back as `original` had them.
	void RestoreEquations(IEquationMgr* equations, const std::vector<std::string>& original)
	{
		long count = 0;
		equations->GetCount(&count);
		for (long i = count - 1; i >= 0; --i)
		{
			long left = 0;
			equations->Delete(i, &left);
		}
		for (const std::string& equation : original)
		{
			long index = -1;
			equations->Add2(-1, FromUtf8(equation), VARIANT_TRUE, &index);
		}
	}

	// Why SolidWorks may have refused `equation`: it gives no reason (Add2
	// returns -1, Status -1), so the quoted names are looked up.
	std::string RefusalHint(IModelDoc2* doc, const std::vector<std::string>& equations, const std::string& equation)
	{
		std::string unknown;
		size_t at = equation.find('"');
		for (bool left = true; at != std::string::npos; left = false)
		{
			const size_t close = equation.find('"', at + 1);
			if (close == std::string::npos)
				break;
			const std::string name = equation.substr(at + 1, close - at - 1);
			const bool dimension = name.find('@') != std::string::npos;
			ATL::CComPtr<IDispatch> param;
			// A new global variable's own name is fine; a dimension must exist.
			const bool known = (left && !dimension) || FindEquation(equations, name) >= 0 ||
				(dimension && SUCCEEDED(doc->Parameter(FromUtf8(name), &param)) && param);
			if (!known)
				unknown += (unknown.empty() ? "\"" : ", \"") + name + "\"";
			at = equation.find('"', close + 1);
		}
		return unknown.empty() ? " (check the syntax)"
			: " (" + unknown + " is neither a global variable nor a dimension of the document)";
	}

	void ListEquations(IModelDoc2* doc, IEquationMgr* equations, sld_mcp::v1::EquationsResponse* response)
	{
		const std::vector<std::string> texts = ReadEquations(equations);
		for (long i = 0; i < static_cast<long>(texts.size()); ++i)
		{
			sld_mcp::v1::EquationInfo* info = response->add_equations();
			info->set_name(EquationName(texts[i]));
			info->set_equation(texts[i]);
			VARIANT_BOOL global = VARIANT_FALSE;
			equations->get_GlobalVariable(i, &global);
			info->set_global(global != VARIANT_FALSE);
			double value = 0.0;
			equations->get_Value(i, &value);
			info->set_value(value);
			if (global)
				continue;
			ATL::CComPtr<IDispatch> param;
			if (FAILED(doc->Parameter(FromUtf8(info->name()), &param)) || !param)
				continue;
			if (ATL::CComQIPtr<IDimension> dim(param.p); dim)
			{
				long type = swDimensionParamTypeUnknown;
				dim->GetType(&type);
				DescribeDimensionValue(dim, info->mutable_dimension());
				info->mutable_dimension()->set_type(type == swDimensionParamTypeDoubleAngular
					? sld_mcp::v1::DIMENSION_TYPE_ANGULAR : sld_mcp::v1::DIMENSION_TYPE_LINEAR);
			}
		}
	}

	grpc::Status ApplyEquations(IModelDoc2* doc, IEquationMgr* equations, const sld_mcp::v1::EquationsRequest& request)
	{
		for (const std::string& name : request.delete_())
		{
			const long index = FindEquation(ReadEquations(equations), name);
			if (index < 0)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"the document has no equation for \"" + name + "\" to delete");
			long left = 0;
			HRESULT hr = equations->Delete(index, &left);
			if (FAILED(hr))
				return ComError("IEquationMgr::Delete", hr);
			if (left < 0)
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks didn't delete the equation for \"" + name + "\"");
		}
		for (const sld_mcp::v1::EquationChange& change : request.set())
		{
			if (change.name().empty() || change.expression().empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "an equation needs a name and an expression");
			const std::string equation = "\"" + change.name() + "\" = " + change.expression();
			const std::vector<std::string> current = ReadEquations(equations);
			const long index = FindEquation(current, change.name());
			long status = 0;
			if (index >= 0)
			{
				HRESULT hr = equations->put_Equation(index, FromUtf8(equation));
				if (FAILED(hr))
					return ComError("IEquationMgr::Equation", hr);
				// A refused change leaves the old text (seen 2026-11-02).
				equations->get_Status(&status);
			}
			else
			{
				HRESULT hr = equations->Add2(-1, FromUtf8(equation), VARIANT_TRUE, &status);
				if (FAILED(hr))
					return ComError("IEquationMgr::Add2", hr);
			}
			if (status < 0)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"SolidWorks refused the equation " + equation + RefusalHint(doc, current, equation));
		}
		return grpc::Status::OK;
	}

	// ---- Design tables: SolidWorks' own Excel worksheet, reached through
	// IDesignTable::EditTable2 and closed with CloseFamilyTable, which applies
	// it (IDesignTable::GetEntryText reads empty strings: seen 2026-11-02).

	// Late-bound call with arguments (Excel's Cells(row, column), ...).
	HRESULT Call(IDispatch* obj, const wchar_t* name, WORD flags, std::vector<ATL::CComVariant> args,
		ATL::CComVariant* result)
	{
		if (!obj)
			return E_POINTER;
		DISPID id = DISPID_UNKNOWN;
		LPOLESTR names[] = { const_cast<LPOLESTR>(name) };
		HRESULT hr = obj->GetIDsOfNames(IID_NULL, names, 1, LOCALE_USER_DEFAULT, &id);
		if (FAILED(hr))
			return hr;
		std::reverse(args.begin(), args.end());  // IDispatch takes them last first
		DISPID put = DISPID_PROPERTYPUT;
		DISPPARAMS params = { args.empty() ? nullptr : &args[0], nullptr, static_cast<UINT>(args.size()), 0 };
		if (flags == DISPATCH_PROPERTYPUT)
		{
			params.rgdispidNamedArgs = &put;
			params.cNamedArgs = 1;
		}
		return obj->Invoke(id, IID_NULL, LOCALE_USER_DEFAULT, flags, &params, result, nullptr, nullptr);
	}

	ATL::CComPtr<IDispatch> Cell(IDispatch* sheet, long row, long column)
	{
		ATL::CComVariant cell;
		if (FAILED(Call(sheet, L"Cells", DISPATCH_METHOD | DISPATCH_PROPERTYGET,
			{ ATL::CComVariant(row), ATL::CComVariant(column) }, &cell)) || cell.vt != VT_DISPATCH)
			return nullptr;
		return cell.pdispVal;
	}

	std::string CellText(IDispatch* sheet, long row, long column)
	{
		ATL::CComVariant text;
		if (FAILED(Call(Cell(sheet, row, column), L"Text", DISPATCH_PROPERTYGET, {}, &text)) || text.vt != VT_BSTR)
			return {};
		return ToUtf8(text.bstrVal);
	}

	bool SetCell(IDispatch* sheet, long row, long column, const std::string& text)
	{
		return SUCCEEDED(Call(Cell(sheet, row, column), L"Value", DISPATCH_PROPERTYPUT,
			{ ATL::CComVariant(FromUtf8(text)) }, nullptr));
	}

	// The table's worksheet, open for editing; closing applies the table.
	class OpenTable
	{
	public:
		OpenTable(IModelDoc2* doc, IDesignTable* table) : m_doc(doc), m_table(table)
		{
			VARIANT_BOOL attached = VARIANT_FALSE;
			if (FAILED(table->Attach(&attached)) || !attached)
				return;
			m_attached = true;
			table->EditTable2(VARIANT_FALSE, &m_sheet);
		}
		~OpenTable()
		{
			if (m_sheet)
				m_doc->CloseFamilyTable();
			if (m_attached)
				m_table->Detach();
		}
		OpenTable(const OpenTable&) = delete;
		OpenTable& operator=(const OpenTable&) = delete;

		IDispatch* Sheet() const { return m_sheet; }

		// Applies the table now; the sheet is gone afterwards.
		void Close()
		{
			if (m_sheet)
				m_doc->CloseFamilyTable();
			m_sheet.Release();
		}

	private:
		ATL::CComPtr<IModelDoc2> m_doc;
		ATL::CComPtr<IDesignTable> m_table;
		ATL::CComPtr<IDispatch> m_sheet;
		bool m_attached = false;
	};

	// Title in A1, the columns in row 2 from B, a configuration per row
	// from row 3 (SolidWorks' layout).
	void ReadSheet(IDispatch* sheet, sld_mcp::v1::DesignTableContent* out)
	{
		for (long column = 2; column < 2 + kMaxColumns; ++column)
		{
			std::string header = CellText(sheet, 2, column);
			if (header.empty())
				break;
			out->add_columns(std::move(header));
		}
		for (long row = 3; row < 3 + kMaxRows; ++row)
		{
			std::string configuration = CellText(sheet, row, 1);
			if (configuration.empty())
				break;
			sld_mcp::v1::DesignTableRow* r = out->add_rows();
			r->set_configuration(std::move(configuration));
			for (int column = 0; column < out->columns_size(); ++column)
				r->add_values(CellText(sheet, row, column + 2));
		}
	}

	grpc::Status GetDesignTable(IModelDoc2* doc, ATL::CComPtr<IDesignTable>& table)
	{
		ATL::CComPtr<IDispatch> disp;
		HRESULT hr = doc->GetDesignTable(&disp);
		if (FAILED(hr))
			return ComError("IModelDoc2::GetDesignTable", hr);
		table = ATL::CComQIPtr<IDesignTable>(disp.p);
		if (!table)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks gives no design table");
		return grpc::Status::OK;
	}

	bool HasDesignTable(IModelDocExtension* ext)
	{
		VARIANT_BOOL has = VARIANT_FALSE;
		return SUCCEEDED(ext->HasDesignTable(&has)) && has;
	}

	bool StartsWith(const std::string& s, const char* prefix)
	{
		return _strnicmp(s.c_str(), prefix, strlen(prefix)) == 0;
	}

	// A column SolidWorks can't use opens a modal error dialog when the
	// table is applied, so the ones that name something are looked up first.
	grpc::Status CheckColumn(IModelDoc2* doc, long docType, const std::vector<std::string>& equations,
		const std::string& column)
	{
		if (column.empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a column has no name");
		if (StartsWith(column, "$VALUE@"))
		{
			const size_t end = column.rfind('@');
			const std::string name = end > 7 ? column.substr(7, end - 7) : std::string();
			if (_stricmp(column.substr(end).c_str(), "@equations") != 0 || name.empty())
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"column \"" + column + "\": a global variable's column is $VALUE@<name>@equations");
			if (FindEquation(equations, name) < 0)
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"column \"" + column + "\": the document has no global variable \"" + name +
					"\" -- make it with the equations tool first");
			return grpc::Status::OK;
		}
		if (StartsWith(column, "$STATE@"))
		{
			const std::string name = column.substr(7);
			const std::vector<TreeFeature> tree = FeatureTree(doc);
			// Components are named "bolt<1>" here; those go through as given.
			if (docType != swDocASSEMBLY &&
				std::none_of(tree.begin(), tree.end(), [&](const TreeFeature& f) { return f.name == name; }))
				return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
					"column \"" + column + "\": the document has no feature \"" + name + "\"");
			return grpc::Status::OK;
		}
		if (column[0] == '$')
			return grpc::Status::OK;  // $DESCRIPTION, $PARENT, $PRP@..., ...: SolidWorks' keywords
		ATL::CComPtr<IDispatch> param;
		if (column.find('@') == std::string::npos || FAILED(doc->Parameter(FromUtf8(column), &param)) ||
			!ATL::CComQIPtr<IDimension>(param.p))
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"column \"" + column + "\" is no dimension of the document (names look like D1@Sketch1) and no "
				"keyword ($STATE@<feature>, $VALUE@<global variable>@equations, $DESCRIPTION, ...)");
		return grpc::Status::OK;
	}

	grpc::Status WriteTable(IModelDoc2* doc, IModelDocExtension* ext, const sld_mcp::v1::DesignTableContent& content,
		sld_mcp::v1::DesignTableResponse* response)
	{
		if (!HasDesignTable(ext))
		{
			doc->InsertFamilyTableNew();
			if (!HasDesignTable(ext))
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks didn't make a design table (it needs Microsoft Excel installed)");
		}
		ATL::CComPtr<IDesignTable> table;
		if (grpc::Status status = GetDesignTable(doc, table); !status.ok())
			return status;
		// LastError keeps the previous write's error otherwise (seen 2026-11-02).
		table->put_LastError(swDTblNoError);
		OpenTable open(doc, table);
		IDispatch* sheet = open.Sheet();
		if (!sheet)
			return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
				"SolidWorks didn't open the design table's worksheet (is Microsoft Excel installed?)");

		// The title stays; a new table's sample row ("First Instance") goes.
		const std::string title = CellText(sheet, 1, 1);
		ATL::CComVariant cells;
		if (FAILED(Call(sheet, L"Cells", DISPATCH_PROPERTYGET, {}, &cells)) || cells.vt != VT_DISPATCH ||
			FAILED(Call(cells.pdispVal, L"ClearContents", DISPATCH_METHOD, {}, nullptr)))
			return grpc::Status(grpc::StatusCode::INTERNAL, "couldn't clear the design table's worksheet");
		bool written = SetCell(sheet, 1, 1, title);
		for (int c = 0; c < content.columns_size(); ++c)
			written = SetCell(sheet, 2, c + 2, content.columns(c)) && written;
		for (int r = 0; r < content.rows_size(); ++r)
		{
			const sld_mcp::v1::DesignTableRow& row = content.rows(r);
			written = SetCell(sheet, r + 3, 1, row.configuration()) && written;
			for (int c = 0; c < row.values_size(); ++c)
				if (!row.values(c).empty())
					written = SetCell(sheet, r + 3, c + 2, row.values(c)) && written;
		}
		if (!written)
			return grpc::Status(grpc::StatusCode::INTERNAL, "Excel didn't take every cell of the design table");
		open.Close();

		long error = 0;
		table->get_LastError(&error);
		response->set_table_error(static_cast<int32_t>(error));
		return grpc::Status::OK;
	}
}

grpc::Status SolidWorksService::Equations(grpc::ServerContext* context,
	const sld_mcp::v1::EquationsRequest* request,
	sld_mcp::v1::EquationsResponse* response)
{
	return m_executor.Execute(context, "Equations", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		long docType = swDocNONE;
		if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
			return status;
		ATL::CComPtr<IEquationMgr> equations;
		if (grpc::Status status = GetEquationMgr(doc, equations); !status.ok())
			return status;

		if (!request->delete_().empty() || !request->set().empty())
		{
			if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
				return status;
			const std::vector<std::string> original = ReadEquations(equations);
			const std::vector<TreeFeature> tree = FeatureTree(doc);
			const auto errorsBefore = FeatureErrors(tree);

			PreferenceToggles toggles(swApp);
			toggles.Set(swShowErrorsEveryRebuild, false);
			grpc::Status applied = ApplyEquations(doc, equations, *request);
			if (!applied.ok())
				RestoreEquations(equations, original);
			// SolidWorks doesn't rebuild on its own (AutomaticRebuild is off).
			VARIANT_BOOL rebuilt = VARIANT_FALSE;
			doc->EditRebuild3(&rebuilt);
			if (!applied.ok())
				return applied;
			AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), response->mutable_new_errors());
		}

		ListEquations(doc, equations, response);
		return grpc::Status::OK;
	});
}

grpc::Status SolidWorksService::DesignTable(grpc::ServerContext* context,
	const sld_mcp::v1::DesignTableRequest* request,
	sld_mcp::v1::DesignTableResponse* response)
{
	using Action = sld_mcp::v1::DesignTableRequest;
	if (request->action_case() == Action::kWrite)
	{
		const auto& write = request->write();
		if (write.columns().empty() || write.rows().empty())
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "a design table needs columns and rows");
		if (write.columns_size() > kMaxColumns || write.rows_size() > kMaxRows)
			return grpc::Status(grpc::StatusCode::INVALID_ARGUMENT,
				"at most " + std::to_string(kMaxColumns) + " columns and " + std::to_string(kMaxRows) + " rows");
	}

	return m_executor.Execute(context, "DesignTable", ExecutionPolicy::CompleteOperation,
		[&](RequestControl& control, ISldWorks* swApp) -> grpc::Status {
		ATL::CComPtr<IModelDoc2> doc;
		long docType = swDocNONE;
		if (grpc::Status status = GetActiveModel(swApp, doc, docType); !status.ok())
			return status;
		ATL::CComPtr<IModelDocExtension> ext;
		if (grpc::Status status = GetExtension(doc, ext); !status.ok())
			return status;

		const ConfigurationList before = ReadConfigurations(doc);
		if (request->action_case() != Action::ACTION_NOT_SET)
		{
			if (grpc::Status status = CheckNoOpenSketch(doc); !status.ok())
				return status;
			const std::vector<TreeFeature> tree = FeatureTree(doc);
			const auto errorsBefore = FeatureErrors(tree);
			PreferenceToggles toggles(swApp);
			toggles.Set(swShowErrorsEveryRebuild, false);
			if (request->action_case() == Action::kWrite)
			{
				ATL::CComPtr<IEquationMgr> equations;
				if (grpc::Status status = GetEquationMgr(doc, equations); !status.ok())
					return status;
				const std::vector<std::string> globals = ReadEquations(equations);
				for (const std::string& column : request->write().columns())
					if (grpc::Status status = CheckColumn(doc, docType, globals, column); !status.ok())
						return status;
				if (grpc::Status status = WriteTable(doc, ext, request->write(), response); !status.ok())
					return status;
			}
			else if (HasDesignTable(ext))
			{
				HRESULT hr = doc->DeleteDesignTable();
				if (FAILED(hr))
					return ComError("IModelDoc2::DeleteDesignTable", hr);
				if (HasDesignTable(ext))
					return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "SolidWorks didn't delete the design table");
			}
			VARIANT_BOOL rebuilt = VARIANT_FALSE;
			doc->EditRebuild3(&rebuilt);
			AddNewErrors(errorsBefore, FeatureErrors(FeatureTree(doc)), response->mutable_new_errors());
		}

		const ConfigurationList after = ReadConfigurations(doc);
		for (const std::string& name : after.names)
			if (std::find(before.names.begin(), before.names.end(), name) == before.names.end())
				response->add_created_configurations(name);
		response->set_active(after.active);
		response->set_exists(HasDesignTable(ext));
		if (response->exists())
		{
			ATL::CComPtr<IDesignTable> table;
			if (grpc::Status status = GetDesignTable(doc, table); !status.ok())
				return status;
			OpenTable open(doc, table);
			if (!open.Sheet())
				return grpc::Status(grpc::StatusCode::FAILED_PRECONDITION,
					"SolidWorks didn't open the design table's worksheet (is Microsoft Excel installed?)");
			ReadSheet(open.Sheet(), response->mutable_table());
		}
		return grpc::Status::OK;
	});
}

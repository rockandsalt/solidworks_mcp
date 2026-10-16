// ComVariant.h : conversions between the VARIANT / SAFEARRAY values the
// SolidWorks API uses for arrays and plain C++ containers.
//
// SolidWorks returns arrays as VARIANTs holding a 1-D SAFEARRAY, usually of
// VT_R8 (coordinates, parameters), VT_DISPATCH (faces, edges, features) or
// VT_UI1 (persist IDs) -- but some calls wrap each element in a VARIANT
// instead, so the readers accept VT_VARIANT elements too. Anything that
// isn't an array (VT_EMPTY, VT_NULL, ...) reads as empty.
#pragma once

#include <cstring>
#include <limits>
#include <string>
#include <vector>

namespace comvariant
{
	namespace detail
	{
		// The SAFEARRAY inside `v`, if it holds a 1-D one.
		inline SAFEARRAY* Array1D(const VARIANT& v)
		{
			if (!(v.vt & VT_ARRAY))
				return nullptr;
			SAFEARRAY* sa = (v.vt & VT_BYREF) ? (v.pparray ? *v.pparray : nullptr) : v.parray;
			if (!sa || SafeArrayGetDim(sa) != 1)
				return nullptr;
			return sa;
		}

		// Calls `f(elements, count)` with the array's data locked.
		template <typename T, typename F>
		void WithData(SAFEARRAY* sa, F&& f)
		{
			LONG lo = 0, hi = -1;
			if (FAILED(SafeArrayGetLBound(sa, 1, &lo)) || FAILED(SafeArrayGetUBound(sa, 1, &hi)) || hi < lo)
				return;
			void* data = nullptr;
			if (FAILED(SafeArrayAccessData(sa, &data)))
				return;
			f(static_cast<const T*>(data), static_cast<size_t>(hi - lo + 1));
			SafeArrayUnaccessData(sa);
		}
	}

	// A numeric array as doubles. Non-numeric VARIANT elements read as NaN.
	inline std::vector<double> ToDoubles(const VARIANT& v)
	{
		std::vector<double> out;
		SAFEARRAY* sa = detail::Array1D(v);
		if (!sa)
			return out;
		switch (v.vt & VT_TYPEMASK)
		{
		case VT_R8:
			detail::WithData<double>(sa, [&](const double* p, size_t n) { out.assign(p, p + n); });
			break;
		case VT_R4:
			detail::WithData<float>(sa, [&](const float* p, size_t n) { out.assign(p, p + n); });
			break;
		case VT_VARIANT:
			detail::WithData<VARIANT>(sa, [&](const VARIANT* p, size_t n) {
				out.reserve(n);
				for (size_t i = 0; i < n; ++i)
				{
					VARIANT d;
					VariantInit(&d);
					if (SUCCEEDED(VariantChangeType(&d, const_cast<VARIANT*>(&p[i]), 0, VT_R8)))
						out.push_back(d.dblVal);
					else
						out.push_back(std::numeric_limits<double>::quiet_NaN());
					VariantClear(&d);
				}
			});
			break;
		}
		return out;
	}

	// An integer array (e.g. a sketch entity ID) as int64s. Non-numeric
	// VARIANT elements read as 0.
	inline std::vector<long long> ToInt64s(const VARIANT& v)
	{
		std::vector<long long> out;
		SAFEARRAY* sa = detail::Array1D(v);
		if (!sa)
			return out;
		switch (v.vt & VT_TYPEMASK)
		{
		case VT_I2:
		case VT_BOOL: // VARIANT_BOOL: -1 (true) / 0
			detail::WithData<SHORT>(sa, [&](const SHORT* p, size_t n) { out.assign(p, p + n); });
			break;
		case VT_I4:
		case VT_INT:
			detail::WithData<LONG>(sa, [&](const LONG* p, size_t n) { out.assign(p, p + n); });
			break;
		case VT_UI4:
		case VT_UINT:
			detail::WithData<ULONG>(sa, [&](const ULONG* p, size_t n) { out.assign(p, p + n); });
			break;
		case VT_I8:
			detail::WithData<LONGLONG>(sa, [&](const LONGLONG* p, size_t n) { out.assign(p, p + n); });
			break;
		case VT_R8:
			detail::WithData<double>(sa, [&](const double* p, size_t n) {
				for (size_t i = 0; i < n; ++i)
					out.push_back(static_cast<long long>(p[i]));
			});
			break;
		case VT_VARIANT:
			detail::WithData<VARIANT>(sa, [&](const VARIANT* p, size_t n) {
				out.reserve(n);
				for (size_t i = 0; i < n; ++i)
				{
					VARIANT d;
					VariantInit(&d);
					if (SUCCEEDED(VariantChangeType(&d, const_cast<VARIANT*>(&p[i]), 0, VT_I8)))
						out.push_back(d.llVal);
					else
						out.push_back(0);
					VariantClear(&d);
				}
			});
			break;
		}
		return out;
	}

	// An array of COM objects (or a single one). Null elements are kept as
	// null pointers so positions stay meaningful.
	inline std::vector<ATL::CComPtr<IDispatch>> ToDispatches(const VARIANT& v)
	{
		std::vector<ATL::CComPtr<IDispatch>> out;
		auto add = [&](IUnknown* unk) {
			ATL::CComPtr<IDispatch> disp;
			if (unk)
				unk->QueryInterface(IID_PPV_ARGS(&disp));
			out.push_back(disp);
		};

		if (v.vt == VT_DISPATCH)
		{
			add(v.pdispVal);
			return out;
		}
		SAFEARRAY* sa = detail::Array1D(v);
		if (!sa)
			return out;
		switch (v.vt & VT_TYPEMASK)
		{
		case VT_DISPATCH:
			detail::WithData<IDispatch*>(sa, [&](IDispatch* const* p, size_t n) {
				for (size_t i = 0; i < n; ++i)
					add(p[i]);
			});
			break;
		case VT_UNKNOWN:
			detail::WithData<IUnknown*>(sa, [&](IUnknown* const* p, size_t n) {
				for (size_t i = 0; i < n; ++i)
					add(p[i]);
			});
			break;
		case VT_VARIANT:
			detail::WithData<VARIANT>(sa, [&](const VARIANT* p, size_t n) {
				for (size_t i = 0; i < n; ++i)
				{
					if (p[i].vt == VT_DISPATCH)
						add(p[i].pdispVal);
					else if (p[i].vt == VT_UNKNOWN)
						add(p[i].punkVal);
					else
						add(nullptr);
				}
			});
			break;
		}
		return out;
	}

	// A string array (or a single string). Non-string VARIANT elements read
	// as empty strings so positions stay meaningful.
	inline std::vector<ATL::CComBSTR> ToBstrs(const VARIANT& v)
	{
		std::vector<ATL::CComBSTR> out;
		if (v.vt == VT_BSTR)
		{
			out.emplace_back(v.bstrVal);
			return out;
		}
		SAFEARRAY* sa = detail::Array1D(v);
		if (!sa)
			return out;
		switch (v.vt & VT_TYPEMASK)
		{
		case VT_BSTR:
			detail::WithData<BSTR>(sa, [&](const BSTR* p, size_t n) {
				for (size_t i = 0; i < n; ++i)
					out.emplace_back(p[i]);
			});
			break;
		case VT_VARIANT:
			detail::WithData<VARIANT>(sa, [&](const VARIANT* p, size_t n) {
				for (size_t i = 0; i < n; ++i)
					out.emplace_back(p[i].vt == VT_BSTR ? p[i].bstrVal : nullptr);
			});
			break;
		}
		return out;
	}

	// An array of VARIANTs (e.g. IDrawingDoc::GetViews: one array of views
	// per sheet) as copies of its elements.
	inline std::vector<ATL::CComVariant> ToVariants(const VARIANT& v)
	{
		std::vector<ATL::CComVariant> out;
		SAFEARRAY* sa = detail::Array1D(v);
		if (!sa || (v.vt & VT_TYPEMASK) != VT_VARIANT)
			return out;
		detail::WithData<VARIANT>(sa, [&](const VARIANT* p, size_t n) {
			out.reserve(n);
			for (size_t i = 0; i < n; ++i)
				out.emplace_back(p[i]);
		});
		return out;
	}

	// A byte array (e.g. a persist ID) as a std::string of raw bytes.
	inline std::string ToBytes(const VARIANT& v)
	{
		std::string out;
		SAFEARRAY* sa = detail::Array1D(v);
		if (!sa)
			return out;
		switch (v.vt & VT_TYPEMASK)
		{
		case VT_UI1:
		case VT_I1:
			detail::WithData<char>(sa, [&](const char* p, size_t n) { out.assign(p, n); });
			break;
		case VT_VARIANT:
			detail::WithData<VARIANT>(sa, [&](const VARIANT* p, size_t n) {
				out.reserve(n);
				for (size_t i = 0; i < n; ++i)
				{
					VARIANT b;
					VariantInit(&b);
					if (SUCCEEDED(VariantChangeType(&b, const_cast<VARIANT*>(&p[i]), 0, VT_UI1)))
						out.push_back(static_cast<char>(b.bVal));
					VariantClear(&b);
				}
			});
			break;
		}
		return out;
	}

	// Raw bytes as a VARIANT holding a SAFEARRAY of VT_UI1, the form
	// SolidWorks takes persist IDs back in.
	inline ATL::CComVariant FromBytes(const std::string& bytes)
	{
		ATL::CComVariant v;
		SAFEARRAY* sa = SafeArrayCreateVector(VT_UI1, 0, static_cast<ULONG>(bytes.size()));
		if (!sa)
			return v;
		void* data = nullptr;
		if (SUCCEEDED(SafeArrayAccessData(sa, &data)))
		{
			if (!bytes.empty())
				std::memcpy(data, bytes.data(), bytes.size());
			SafeArrayUnaccessData(sa);
		}
		v.vt = VT_ARRAY | VT_UI1;
		v.parray = sa; // owned by v from here on
		return v;
	}

	// Doubles as a VARIANT holding a SAFEARRAY of VT_R8.
	inline ATL::CComVariant FromDoubles(const double* values, size_t count)
	{
		ATL::CComVariant v;
		SAFEARRAY* sa = SafeArrayCreateVector(VT_R8, 0, static_cast<ULONG>(count));
		if (!sa)
			return v;
		void* data = nullptr;
		if (SUCCEEDED(SafeArrayAccessData(sa, &data)))
		{
			std::memcpy(data, values, count * sizeof(double));
			SafeArrayUnaccessData(sa);
		}
		v.vt = VT_ARRAY | VT_R8;
		v.parray = sa;
		return v;
	}
}

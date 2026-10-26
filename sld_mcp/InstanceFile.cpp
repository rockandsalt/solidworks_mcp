// InstanceFile.cpp : the instance file the MCP server finds this add-in by.
#include "pch.h"
#include "InstanceFile.h"
#include <bcrypt.h>
#include <cstdio>
#include <ctime>
#include <stdexcept>

namespace
{
	std::string Win32Error(const char* what)
	{
		return std::string(what) + " (error " + std::to_string(GetLastError()) + ")";
	}

	// %LOCALAPPDATA%\sld_mcp\instances, created if missing. The MCP server
	// reads the same variable.
	std::wstring InstancesDirectory()
	{
		wchar_t base[MAX_PATH] = {};
		DWORD len = GetEnvironmentVariableW(L"LOCALAPPDATA", base, MAX_PATH);
		if (len == 0 || len >= MAX_PATH)
			throw std::runtime_error("LOCALAPPDATA isn't set, so there's nowhere to publish the port");

		std::wstring dir = base;
		for (const wchar_t* part : { L"\\sld_mcp", L"\\instances" })
		{
			dir += part;
			if (!CreateDirectoryW(dir.c_str(), nullptr) && GetLastError() != ERROR_ALREADY_EXISTS)
				throw std::runtime_error(Win32Error("couldn't create the sld_mcp instances folder"));
		}
		return dir;
	}

	std::string UtcNow()
	{
		std::time_t now = std::time(nullptr);
		std::tm utc = {};
		gmtime_s(&utc, &now);
		char text[32] = {};
		std::strftime(text, sizeof(text), "%Y-%m-%dT%H:%M:%SZ", &utc);
		return text;
	}
}

std::string NewToken()
{
	unsigned char bytes[16] = {};
	if (!BCRYPT_SUCCESS(BCryptGenRandom(nullptr, bytes, sizeof(bytes), BCRYPT_USE_SYSTEM_PREFERRED_RNG)))
		throw std::runtime_error("couldn't generate the connection token");

	std::string token;
	for (unsigned char byte : bytes)
	{
		char hex[3] = {};
		std::snprintf(hex, sizeof(hex), "%02x", byte);
		token += hex;
	}
	return token;
}

InstanceFile::InstanceFile(int port, const std::string& token)
{
	const DWORD pid = GetCurrentProcessId();
	const std::wstring dir = InstancesDirectory();
	m_path = dir + L"\\" + std::to_wstring(pid) + L".json";

	const std::string json = "{\"pid\": " + std::to_string(pid) + ", \"port\": " + std::to_string(port) +
		", \"token\": \"" + token + "\", \"started\": \"" + UtcNow() + "\"}\n";

	// Written next to it and renamed into place, so a reader never sees half
	// a file.
	const std::wstring temp = m_path + L".tmp";
	HANDLE file = CreateFileW(temp.c_str(), GENERIC_WRITE, 0, nullptr, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, nullptr);
	if (file == INVALID_HANDLE_VALUE)
		throw std::runtime_error(Win32Error("couldn't write the sld_mcp instance file"));
	DWORD written = 0;
	const BOOL ok = WriteFile(file, json.data(), static_cast<DWORD>(json.size()), &written, nullptr);
	CloseHandle(file);
	if (!ok || written != json.size() || !MoveFileExW(temp.c_str(), m_path.c_str(), MOVEFILE_REPLACE_EXISTING))
	{
		std::string error = Win32Error("couldn't write the sld_mcp instance file");
		DeleteFileW(temp.c_str());
		throw std::runtime_error(error);
	}
}

InstanceFile::~InstanceFile()
{
	DeleteFileW(m_path.c_str());
}

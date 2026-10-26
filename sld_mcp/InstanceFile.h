// InstanceFile.h : tells the MCP server where this add-in listens.
//
// The gRPC server takes whatever free port the OS gives it, so the add-in
// publishes it in %LOCALAPPDATA%\sld_mcp\instances\<SolidWorks pid>.json,
// together with a random token every call has to carry (see RpcServer):
//
//   {"pid": 12345, "port": 61873, "token": "<32 hex digits>", "started": "2026-10-25T10:12:00Z"}
//
// The MCP server uses the newest file whose pid is still a running
// SolidWorks and deletes the ones left behind by a crash. The format must
// match mcp_server/src/discovery.rs.
#pragma once

#include <string>

// A fresh token: 128 bits from the system RNG as 32 lowercase hex digits.
// Throws std::runtime_error.
std::string NewToken();

class InstanceFile
{
public:
	// Writes the file, replacing one a crashed SolidWorks with the same pid
	// left behind. Throws std::runtime_error.
	InstanceFile(int port, const std::string& token);
	// Deletes it.
	~InstanceFile();

	InstanceFile(const InstanceFile&) = delete;
	InstanceFile& operator=(const InstanceFile&) = delete;

private:
	std::wstring m_path;
};

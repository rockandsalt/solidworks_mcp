# solidworks_mcp

An MCP server that lets an AI model drive SOLIDWORKS. Two halves:

```
MCP client ──stdio──▶ mcp_server (Rust, rmcp) ──gRPC 127.0.0.1:50551──▶ sld_mcp.dll (C++ ATL add-in inside SOLIDWORKS) ──COM──▶ ISldWorks
```

- `proto/sld_mcp.proto`: the IPC contract, single source of truth.
- `mcp_server/`: the MCP tools (`src/main.rs`), gRPC client, and everything 
  that can live outside SolidWorks: units (tools take mm, the wire is meters),
  id registry, enum names, JSON, filtering, geometry math, rendering.
- `sld_mcp/`: the add-in. `SolidWorksService.cpp` implements the RPCs with
  COM calls. Keep it thin: logic goes to the Rust side.

Design notes, per area, with each feature's "Untested in SolidWorks —
verify: …" list: `docs/notes/` (architecture.md first; also diagnosing,
deleting, drawings, installer, devloop). Read the relevant one before changing
an area, and update it when you change the design or learn how SolidWorks
actually behaves.

## Solidworks
Solidworks can be started with the following command : 

```powershell
"C:\Program Files\Dassault Systemes\SOLIDWORKS 3DEXPERIENCE R2026x\win_b64\code\bin\CATSTART.exe" -run "SWXDesktopLauncher.exe" -object "--AppName=\"SWXCSWK_AP\" -tenant=OI000596756 -monoapp -3DRegistryURL=https://eu1-makers-registry.3dexperience.3ds.com -LaunchAppDir=\"C:\Program Files\Dassault Systemes\SOLIDWORKS Apps 2026\SOLIDWORKS\" " -nowindow
```
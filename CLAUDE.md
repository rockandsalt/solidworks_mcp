# solidworks_mcp

An MCP server that lets an AI model drive SOLIDWORKS. Two halves:

```
MCP client ──stdio──▶ mcp_server (Rust, rmcp) ──gRPC 127.0.0.1:50551──▶ sld_mcp.dll (C++ ATL add-in inside SOLIDWORKS) ──COM──▶ ISldWorks
```

- `proto/sld_mcp.proto`: the IPC contract, single source of truth.
- `mcp_server/`: the MCP tools (`src/main.rs`), gRPC client, and everything 
  that can live outside SolidWorks: units (tools take mm, the wire is meters),
  id registry, enum names, JSON, filtering, geometry math, rendering.
- `sld_mcp/`: the add-in. `service/` implements the RPCs with COM calls, one
  file per area (`Sketches.cpp`, `Drawings.cpp`, ...), helpers shared between
  areas in `service/*Helpers.h` and `Common.h`. Keep it thin: logic goes to
  the Rust side.

Design notes, per area, with each feature's "Untested in SolidWorks —
verify: …" list: `docs/notes/` (architecture.md first; also diagnosing,
deleting, editing, drawings, gdt, files, reference, installer, devloop). Read the relevant one before changing
an area, and update it when you change the design or learn how SolidWorks
actually behaves.

## Testing in SolidWorks (dev loop)
Verify changes against real SolidWorks yourself; starting and closing solidworks
is slow and computationaly expensive. Consider doing as many test as possible in a single session. details in
`docs/notes/devloop.md`:

- `devloop\cycle.ps1 -Build all|addin|server|none -Scenario smoke,...`:
  stop SolidWorks if the add-in is rebuilt (it locks the DLL), build, start
  SolidWorks, run scenarios. Exit code 0 = everything passed.
- `devloop\sw.ps1 status|start|stop|reset|screenshot`: SolidWorks control.
- `py -3 devloop\mcp_client.py call <tool> '{json}'` for a one-off call;
  scenarios in `devloop\scenarios\*.py`. Output (transcript, server log,
  PNGs) goes to `devloop\out\`.
- `devloop\agent.ps1 "<task>" [-Resume]`: a headless Claude session with only
  the MCP tools tries a plain-language task and reports what was confusing or
  broken (costs API money, ~$0.50 a task).

Run the PowerShell scripts with `powershell.exe -NoProfile -ExecutionPolicy
Bypass -File <absolute path>` (Windows PowerShell 5.1: COM needs it).

## Solidworks
Solidworks can be started with the following command (`devloop\sw.ps1 start` runs it): 

```powershell
"C:\Program Files\Dassault Systemes\SOLIDWORKS 3DEXPERIENCE R2026x\win_b64\code\bin\CATSTART.exe" -run "SWXDesktopLauncher.exe" -object "--AppName=\"SWXCSWK_AP\" -tenant=OI000596756 -monoapp -3DRegistryURL=https://eu1-makers-registry.3dexperience.3ds.com -LaunchAppDir=\"C:\Program Files\Dassault Systemes\SOLIDWORKS Apps 2025\SOLIDWORKS\" " -nowindow
```
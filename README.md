# SolidWorks MCP

An [MCP](https://modelcontextprotocol.io) server that lets AI assistants such as
Claude and Codex model in SOLIDWORKS: sketch, build and edit features,
assemble components, make drawings, and inspect the result.

```
MCP client ──stdio──▶ mcp_server.exe ──gRPC (localhost)──▶ sld_mcp.dll (SOLIDWORKS add-in) ──COM──▶ SOLIDWORKS
```

## Requirements

- Windows 10/11, 64-bit
- SOLIDWORKS 2025 or newer

## Install

1. Download `sld_mcp-setup-<version>.exe` from
   [Releases](../../releases) and run it.
2. In the installer, tick **Claude Desktop** and/or **Codex** to register the
   server with them.
3. Start SOLIDWORKS. The add-in loads at startup.

For other MCP clients, point them at `C:\Program Files\SolidWorks MCP\mcp_server.exe`
(stdio, no arguments). For example, with Claude Code:

```
claude mcp add solidworks "C:\Program Files\SolidWorks MCP\mcp_server.exe"
```

Clients with tool search (Claude Code defers MCP tools by default) only load
the tools a task needs. For a client without it, list fewer tools (a smaller
prompt) by starting the server on a subset:
`--modes modeling,assembly,drawing` (any of the three) and/or `--inspect-only`
(tools that change nothing). For example, a drawings-only server:

```
claude mcp add solidworks-drawing -- "C:\Program Files\SolidWorks MCP\mcp_server.exe" --modes drawing
```

## What it can do

- **Documents**: new parts, assemblies and drawings; open, save, export, import
  STEP / IGES / Parasolid; units; undo.
- **Sketches**: lines, arcs, circles, splines, slots, polygons, ellipses;
  trim, extend, offset, mirror, pattern; dimensions and relations.
- **Features**: extrude, revolve, sweep, loft (boss and cut), Hole Wizard,
  fillet, chamfer, shell, rib, draft, mirror and patterns; edit, reorder,
  suppress, roll back or delete them.
- **Reference geometry**: planes, axes, points, coordinate systems.
- **Assemblies**: insert, replace, fix and move components; mates and their
  limits; mobility and interference checks.
- **Configurations, equations and design tables.**
- **Drawings**: sheets, standard, projected, section, detail and auxiliary
  views; dimensions, model items, notes, tables, balloons, title blocks.
- **Annotations**: GD&T (feature control frames, datums), surface finish and
  weld symbols, center marks and centerlines.
- **Inspection**: feature tree, sketches, mass properties, measurements,
  rebuild diagnostics, feature recognition on imported bodies, and rendered
  images of models and drawing sheets.
- **`run_macro`**: runs VBA inside SOLIDWORKS for anything the other tools
  don't cover.

## Build from source

Needs Visual Studio 2026 with C++ (x64), vcpkg, Rust, and SOLIDWORKS
installed. For the installer you also need NSIS.

```powershell
.\build.ps1                      # add-in (Debug) + server
.\build.ps1 -Target register     # register the add-in (admin)
.\build.ps1 -Target installer    # Release builds + setup .exe in installer\out
```

Every CAD RPC runs through `CadExecutor`, which owns serialized admission,
COM setup and teardown, cancellation translation, and lifecycle timing. Basic
document queries share the same gate as geometry and edits. The Rust request
interceptor supplies the 60 s deadline for every RPC, preserving explicit
shorter deadlines.

Admitted work runs on SolidWorks' main thread (`MainThread`, a message-only
window the gRPC worker posts to), so its COM calls are direct. From the worker,
each COM call would be marshaled to that thread on its own, at 1-12 ms each;
over the devloop scenarios the whole run is about 5x faster this way. Setting
`SLD_MCP_CAD_THREAD=worker` in SolidWorks' environment runs the work on the
worker as before, to compare.

Expired queued requests never start CAD work. Inspections use `Cancellable`
policy and call the transport-independent `RequestControl::Checkpoint()` in
long traversals; cancelled reads cannot publish a successful partial result.
Commands use `CompleteOperation`: once admitted, they finish their existing
operation and cleanup even if the client stops waiting. Cancellation cannot
interrupt an in-progress COM call or release its gate early. Consequently a
timed-out command may still complete; do not automatically retry it.

For a new RPC, validate arguments, then return `m_executor.Execute(...)` with
the appropriate policy and a callback containing the CAD work. Keep COM objects
inside that callback; handlers and geometry helpers must not acquire their own
model lock, initialize COM, or retain a gRPC context, and the callback must not
wait for another thread: it holds SolidWorks' main thread. Lifecycle diagnostics go
to `OutputDebugString` (debugger/DebugView): process/request IDs, method, phase,
elapsed milliseconds, final status, and whether the deadline/cancellation was
observed. A request without a finished event may still be executing inside COM.

The execution regression checks coverage of all protocol operations, admission,
inspection cancellation, edit completion, exception cleanup, and expired mesh,
macro and document RPCs. It uses fake work/connections and the production RPC
handlers, without SolidWorks running (installed C++ dependencies are required):

```powershell
.\build.ps1 -Target test -SwTypeLibDir 'C:\Program Files\SOLIDWORKS Corp\SOLIDWORKS (2)'
```

The test target builds Release into a separate output directory and can run
while SolidWorks is open. Omit `-SwTypeLibDir` to use the project's default
type-library location. The C++ tests remain in `sld_mcp/tests/`; Python
scenarios under `devloop/scenarios/` exercise a running SolidWorks instance.

Opt-in cancellation checks against a running **rebuilt** add-in (no CAD edits):

```powershell
cargo test --manifest-path mcp_server/Cargo.toml expired_queued_macro_never_launches -- --ignored --nocapture
cargo test --manifest-path mcp_server/Cargo.toml expired_mesh_releases_model_lock -- --ignored --nocapture
```

Run these sequentially with no other clients issuing requests. The second
check needs an active detailed model whose mesh takes more than one second;
it checks that geometry inspection works again after the mesh expires. Loading
a rebuilt add-in requires restarting SolidWorks; preserve unsaved work first.

## License

[AGPL-3.0](LICENSE)

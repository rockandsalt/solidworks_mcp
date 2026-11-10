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

## License

[AGPL-3.0](LICENSE)

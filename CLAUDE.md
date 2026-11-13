# solidworks_mcp

An MCP server that lets an AI model drive SOLIDWORKS.

- `proto/sld_mcp.proto`: the IPC contract, single source of truth.
- `mcp_server/`: the MCP tools (`src/main.rs`), gRPC client, and everything 
  that can live outside SolidWorks.
- `sld_mcp/`: the add-in. `service/` implements the RPCs with COM calls, one
  file per area (`Sketches.cpp`, `Drawings.cpp`, ...), helpers shared between
  areas in `service/*Helpers.h` and `Common.h`. Keep it thin: logic goes to
  the Rust side.

Design notes, per area, with "Untested in SolidWorks —
verify: …" lists where checks are open: `docs/notes/`, a separate private git
repo cloned there and gitignored here, so commit note changes in it (architecture.md first; also diagnosing,
deleting, editing, drawings, gdt, dimxpert, files, units, reference, configurations, equations, measure, installer, devloop, scenarios). Read the relevant one before changing
an area, and update it when you change the design or learn how SolidWorks
actually behaves.

## coding standard
- Simplicity First
- Surgical changes
- Define success criteria. Loop until verified.

## Testing in SolidWorks (dev loop)
Verify changes against real SolidWorks yourself; starting and closing solidworks
is slow and computationally expensive. Only add-in rebuilds force a restart, so batch C++ changes and do as many tests as possible in a single session. Details in
`docs/notes/devloop.md`. A passing `mcp_client.py run` closes its documents and deletes its saved files; after anything else (one-off calls, a failed run, `--no-reset`, `agent.ps1`), run `devloop\sw.ps1 reset` when you are finished to reduce memory consumption.


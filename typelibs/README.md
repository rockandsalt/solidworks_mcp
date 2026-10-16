# SOLIDWORKS type libraries (optional)

Put `sldworks.tlb`, `swpublished.tlb` and `swconst.tlb` from the **oldest**
SOLIDWORKS release the add-in should support (currently 2025) in this folder.
They're in that release's install folder, next to `SLDWORKS.exe`.

When `sldworks.tlb` is here, `sld_mcp.vcxproj` imports these files instead of
the installed release's (`SwTypeLibDir`). The build then fails on any API newer
than that release, and the resulting DLL loads in that release and every newer
one, because the SOLIDWORKS API is backward compatible. Without them, the build
uses the installed SOLIDWORKS (2026), which works as long as the code only
calls APIs that 2025 already had.

Rebuild from clean after adding or changing them (`.\build.ps1 -Clean`, then
build), so the precompiled header is regenerated from the new files.

The `.tlb` files are Dassault's, so `*.tlb` is gitignored; don't commit them
to a public repository.

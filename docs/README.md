# Developer Docs

This directory now keeps a deliberately small set of canonical developer
documents. Planning and backlog state now live in Pulse rather than Markdown
plan files. Anything narrow, historical, or investigatory should live in `tmp/`
only when a durable canonical doc is not the right fit.

Machine-consumed check allowlists do not live here anymore. They now live under
`scripts/internal/check/allowlists/` so `docs/` stays human-facing.

## Canonical docs

- `docs/ENV_VARS.md`
  - environment variable reference, release lifecycle/operator commands, and
    safety notes
- `docs/DATABASE_MIGRATIONS.md`
  - SQLite current-format change/validation contract and the active alpha
    override of legacy compatibility and migration requirements
- `docs/TEST.md`
  - development workflow, RC/stable promotion policy, validation gates, and
    test suite map
- `docs/GPUI_BROWSER_PERFORMANCE.md`
  - reproducible 20k browser projection CPU workload and host-scoped regression baseline
- [Reson audition resampling](RESON_RESAMPLING.md)
  - rate-conversion quality and realtime contracts with the reproducible linear baseline
- `docs/TARGET.md`
  - product target, architecture ownership, runtime contracts, recovery rules,
    automation surfaces, data formats, UI/performance direction, and the
    Yield/Radiant ownership boundary
- `docs/TROUBLESHOOTING.md`
  - common failure modes, diagnostics, and guardrail-change workflow
- `docs/book/src/`
  - public mdBook source for user-facing Yield documentation

- [Visual WGSL effect contract](GPUI_VISUAL_EFFECT_CONTRACT.md)
  - shipped presets, custom source approval, renderer lifetime and native verification limits

## Live operational files

- Planning and backlog
  - live in the existing Pulse project `Yield` at <http://localhost:7437/>;
    the active release roadmap starts at `0.1` (macOS browsing and auditioning)
    and separates later capabilities into `0.2` through `0.7`
  - imported Linear issue identifiers, descriptions, acceptance criteria and
    source links remain traceable; Linear and the old version labels are retained
    as source history
- Public docs build
  - build locally with `mdbook build` or `bash scripts/check.sh mdbook`
  - preview locally with `mdbook serve --hostname 127.0.0.1 --port 3011`
  - generated output belongs under `target/mdbook/` locally and is published
    through the PortalSurfer site path `/yield/docs/`
- `tmp/database_system_audit_plan.md`
  - database-system audit notes and follow-up context
- `tmp/source_runtime_test_isolation_audit_plan.md`
  - source-runtime test-isolation audit notes and follow-up context

## Default workflow

1. Run request preflight.
   - Windows PowerShell:
     `powershell -ExecutionPolicy Bypass -File scripts/agent.ps1 request`
   - macOS/Linux/WSL:
     `bash scripts/agent.sh request`
2. Use `docs/TEST.md` for the right validation lane.
3. Use `docs/TROUBLESHOOTING.md` when a guardrail or environment check fails.
4. Keep changes small, update `docs/TARGET.md` when durable product,
   architecture, runtime, recovery, automation, or data-format contracts change,
   and avoid creating one-off docs unless the information truly needs to live
   separately.

## Principles for this folder

- Prefer a few strong documents over many narrow notes.
- Prefer current contracts over historical execution diaries.
- Keep active planning and task hierarchy in Pulse, not Markdown plan files.
- Use `tmp/` only for narrow investigations or temporary implementation notes.
- Delete stale docs instead of keeping them as archaeological layers.

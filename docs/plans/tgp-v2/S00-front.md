# TGP — Toastty Graphics Protocol (Design v2)

- **Status:** Design v2 — all ambiguities resolved (see `tgp-decisions.md`, `tgp-reconciliation.md`); ready for implementation planning.
- **Date:** 2026-05-31
- **Supersedes:** `2026-05-29-tgp-design.md` (v1 draft).
- **Binding decisions:** clean break from RGP (no adapter); per-app namespace token; placeholder-cells-authoritative inline viewports; sub-captures-viewport-cells input routing. Full rationale in `tgp-reconciliation.md`.

> This is a clean-break protocol. The legacy RGP (`ratty;g;`) implementation remains as independent, untouched code; TGP neither bridges to nor inherits from it.

---



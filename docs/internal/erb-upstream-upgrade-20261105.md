# ERB upstream release integration, 2026-11-05

## Source and scope

- Canonical fork: `easyrentbali/zeabur`, existing sibling checkout `_projects/zeabur`.
- Integration target: existing `erb-dashboard-install` branch, starting at `364545c6d9284d4380dd072f6e20493692cd0df0`.
- Official release: `zeabur-oss/zeabur` `v0.2.0-beta.1`, exact commit `944f737d7594f0a1305b4bc74ffd54275e916a7f`.
- This is a merge preserving all seven published ERB commits. It retains administrator-authorized dashboard installation, encrypted expiring plans, create-only resource ownership and readable service names.
- Scoped integration in the existing canonical fork was assigned explicitly. No clone, nested repository, task workspace or JJ initialization was created. The sibling fork is not LayerFS-registered; this work uses the canonical integration exception, not a verified task mount.

## Integration checklist

- [x] Verify clean fork, branch, remote and exact official release.
- [x] Merge official source without rebasing published history.
- [x] Resolve shared Command import and catalog v3 contribution target.
- [x] Port typed input generators, Pod Security and first-chart input defaults.
- [x] Add regressions for unknown schema, contribution identity and manifest-before-chart inputs.
- [x] Focused frontend tests: 8 pass; full TypeScript typecheck passes.
- [x] Installer sheet formatting and whitespace checks pass.
- [ ] Rust catalog/API tests and pinned nightly formatting: unavailable locally because Nix and the pinned nightly are absent; the Rust registry is empty. Validate in the reviewed CI build before deployment.
- [ ] Full frontend suite: 791 pass; the catalog fixture suite cannot load without the Nix-provided `ZEABUR_CATALOG` directory.
- [ ] Build/deploy and authenticated hosted installation acceptance belong to the owning infrastructure task; this integration alone does not establish them.

## Local validation

Dependencies were restored with `bun install --frozen-lockfile --ignore-scripts`; neither lockfile changed beyond the official upstream merge. No local server or component installation was started. `bun test src/model/component-install.test.ts src/model/service-name.test.ts`, `bun run typecheck`, `bun run fmt:check src/components/installer-sheet.tsx` and `git diff --check` pass. The full `bun test src/` run reports 791 passes and a missing catalog fixture directory. The attempted offline catalog test stops at the missing nightly compiler before compilation; it is not a passing Rust check.

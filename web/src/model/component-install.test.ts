import { expect, test } from "bun:test"

import {
  canApplyComponentPlan,
  componentInstallationPath,
  installationIsComplete,
  missingComponentInputs,
  providedComponentInputs,
  nextInstallationToken,
} from "./component-install"

test("installation paths require a catalog capability and component", () => {
  expect(componentInstallationPath("logs/victoria-logs")).toBe(
    "/api/v1/cluster/components/logs/victoria-logs/installation",
  )
  for (const value of ["logs", "logs/../secrets", "logs/a/b", "logs/%2e%2e", ""]) {
    expect(() => componentInstallationPath(value)).toThrow()
  }
})

test("required fields ignore generated inputs but reject whitespace-only supplied values", () => {
  const inputs = [
    { name: "bucket", description: "Bucket", required: true, generated: false },
    { name: "key", description: "Key", required: true, generated: true },
    { name: "optional", description: "Optional", required: false, generated: false },
  ]
  expect(missingComponentInputs(inputs, { bucket: "  " })).toEqual(["bucket"])
  expect(missingComponentInputs(inputs, { bucket: "backups" })).toEqual([])
})

test("only known nongenerated values are sent, without altering secret contents", () => {
  const inputs = [
    { name: "credentials", description: "Credentials", required: true, generated: false },
    { name: "generated", description: "Generated", required: true, generated: true },
  ]
  expect(
    providedComponentInputs(inputs, {
      credentials: "  synthetic fixture with meaningful whitespace\n",
      generated: "must not override the generated key",
      unrecognized: "must not be sent",
    }),
  ).toEqual({ credentials: "  synthetic fixture with meaningful whitespace\n" })
})

test("submission, pending and external ownership never mean the dashboard installed it", () => {
  for (const state of ["submitted", "notInstalled", "pending", "external", "failed"]) {
    expect(installationIsComplete(state)).toBe(false)
  }
  expect(installationIsComplete("installed")).toBe(true)
})

test("only a current matching plan with writes can be applied", () => {
  const now = Date.parse("2026-11-04T04:00:00Z")
  const plan = {
    component: "logs/victoria-logs",
    profile: "multi-node",
    token: "synthetic-plan-token",
    expiresAt: "2026-11-04T04:05:00Z",
    writes: [{ kind: "HelmChart", name: "logs", namespace: "kube-system" }],
  }
  expect(canApplyComponentPlan(plan, plan.component, plan.profile, now)).toBe(true)
  expect(canApplyComponentPlan(plan, "ingress/traefik", plan.profile, now)).toBe(false)
  expect(canApplyComponentPlan(plan, plan.component, "single-node", now)).toBe(false)
  expect(canApplyComponentPlan({ ...plan, expiresAt: "invalid" }, plan.component, plan.profile, now)).toBe(false)
  expect(canApplyComponentPlan({ ...plan, expiresAt: "2026-11-04T04:00:00Z" }, plan.component, plan.profile, now)).toBe(false)
  expect(canApplyComponentPlan({ ...plan, token: "" }, plan.component, plan.profile, now)).toBe(false)
  expect(canApplyComponentPlan({ ...plan, writes: [] }, plan.component, plan.profile, now)).toBe(false)
})

test("only server-issued pending continuations may advance and submitted tokens never replay", () => {
  const state = { state: "pending", continuationToken: "synthetic-next-phase", phase: 1, totalPhases: 3 }
  expect(nextInstallationToken(state, new Set())).toBe(state.continuationToken)
  expect(nextInstallationToken(state, new Set([state.continuationToken]))).toBeUndefined()
  for (const status of ["installed", "external", "failed", "notInstalled"]) {
    expect(nextInstallationToken({ ...state, state: status }, new Set())).toBeUndefined()
  }
  expect(nextInstallationToken({ ...state, continuationToken: null }, new Set())).toBeUndefined()
  expect(nextInstallationToken({ ...state, phase: 3 }, new Set())).toBeUndefined()
  expect(nextInstallationToken({ ...state, resumeAvailable: true }, new Set())).toBeUndefined()
})

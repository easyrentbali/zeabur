import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"
import { useEffect, useRef, useState } from "react"

import {
  applyComponentInstall,
  getComponentInstallation,
  planComponentInstall,
  type ComponentInstallRequest,
  type ComponentProfile,
} from "@/api/component-install"
import { capabilitiesQuery } from "@/api/queries"
import { PlanDiffs } from "@/components/cluster-plan"
import { clock, useSecondsLeft } from "@/components/plan/expiry"
import { Problem } from "@/components/problem"
import {
  canApplyComponentPlan,
  installationIsComplete,
  missingComponentInputs,
  providedComponentInputs,
} from "@/model/component-install"
import type { Needed } from "@/model/installer"
import { Button } from "@zeabur-oss/design-system/components/button"

/** Actual installation uses the signed-in administrator and an expiring server-reviewed plan. */
export function ComponentInstaller({ components }: { components: Needed[] }) {
  const [selected, setSelected] = useState(components[0]?.id)
  const component = components.find((entry) => entry.id === selected) ?? components[0]
  if (!component) return <p role="alert">Choose a component from the catalog.</p>
  return (
    <section className="flex flex-col gap-4" aria-label="Install from dashboard">
      {components.length > 1 ? (
        <div className="flex flex-wrap gap-2" aria-label="Components to install">
          {components.map((entry) => (
            <Button
              key={entry.id}
              size="sm"
              variant={component.id === entry.id ? "default" : "outline"}
              onClick={() => setSelected(entry.id)}
            >
              {entry.name}
            </Button>
          ))}
        </div>
      ) : null}
      <InstallOne key={component.id} component={component} />
    </section>
  )
}

function InstallOne({ component }: { component: Needed }) {
  const queryClient = useQueryClient()
  const [profile, setProfile] = useState<ComponentProfile>("multi-node")
  const [values, setValues] = useState<Record<string, string>>({})
  const [showValues, setShowValues] = useState(false)
  const [preview, setPreview] = useState<{ sequence: number; request: ComponentInstallRequest }>()
  const [attempted, setAttempted] = useState(false)
  const attemptLock = useRef(false)
  const sequence = useRef(0)
  const installation = useQuery({
    queryKey: ["cluster", "component-install", component.id],
    queryFn: ({ signal }) => getComponentInstallation(component.id, signal),
    retry: false,
    refetchInterval: (query) => {
      if (query.state.status === "error") return false
      const state = query.state.data?.state
      return state === "pending" || (attempted && state === "notInstalled") ? 3000 : false
    },
  })
  const info = installation.data
  useEffect(() => {
    if (info?.state === "installed") {
      void queryClient.invalidateQueries({ queryKey: capabilitiesQuery.queryKey })
    }
  }, [info?.state, queryClient])
  const effectiveProfile = info?.profiles.includes(profile)
    ? profile
    : (info?.profiles[0] ?? profile)
  const plan = useQuery({
    // Keep entered credentials out of query keys, persisted URLs and diagnostics.
    queryKey: ["cluster", "component-install-plan", component.id, preview?.sequence],
    queryFn: ({ signal }) => planComponentInstall(preview!.request, signal),
    enabled: preview !== undefined && !attempted,
    retry: false,
    refetchOnWindowFocus: false,
    refetchOnReconnect: false,
    staleTime: Infinity,
    gcTime: 0,
  })
  const apply = useMutation({
    mutationFn: applyComponentInstall,
    retry: false,
    onSettled: () => {
      setValues({})
      setShowValues(false)
      setPreview(undefined)
      void queryClient.invalidateQueries({ queryKey: ["cluster", "component-install", component.id] })
      void queryClient.invalidateQueries({ queryKey: capabilitiesQuery.queryKey })
    },
  })
  const secondsLeft = useSecondsLeft(plan.data?.expiresAt)
  const missing = missingComponentInputs(info?.inputs ?? [], values)
  const complete = installationIsComplete(info?.state ?? "")
  const canReview = Boolean(info?.installable && !attempted && !plan.isFetching && missing.length === 0)
  const canApply = Boolean(
    info?.installable &&
      !attempted &&
      !plan.isFetching &&
      canApplyComponentPlan(plan.data, component.id, effectiveProfile),
  )

  function changed(next: Record<string, string>) {
    setValues(next)
    setPreview(undefined)
  }

  function review() {
    if (!canReview || !info) return
    setPreview({
      sequence: ++sequence.current,
      request: {
        component: component.id,
        profile: effectiveProfile,
        inputs: providedComponentInputs(info.inputs, values),
      },
    })
  }

  function install() {
    if (!canApply || !plan.data || attemptLock.current) return
    // A lost response must never automatically submit the same installation twice.
    attemptLock.current = true
    setAttempted(true)
    apply.mutate(plan.data.token)
  }

  return (
    <>
      <div className="flex flex-col gap-1">
        <h3 className="font-medium">Install {component.name}</h3>
        <p className="type-meta text-pretty text-muted-foreground">
          Review the changes, then install using your signed-in permissions. Existing components
          are preserved.
        </p>
      </div>
      {installation.isPending ? <p role="status">Checking installation and permissions…</p> : null}
      {installation.isError ? (
        <Problem
          error={installation.error}
          action={
            <Button variant="outline" size="sm" onClick={() => void installation.refetch()}>
              Check again
            </Button>
          }
        />
      ) : null}
      {info ? (
        <>
          <div className="rounded-lg border bg-muted/20 p-3" role="status" aria-live="polite">
            <p className="font-medium">
              {complete
                ? "Installed and healthy"
                : info.state === "external"
                  ? "Already installed outside this dashboard"
                  : info.state === "failed"
                    ? "Installation needs attention"
                    : info.state === "pending" || apply.isPending || attempted
                      ? "Checking installation progress"
                      : info.installable ? "Ready to review" : "Installation unavailable"}
            </p>
            {info.message ? <p className="mt-1 type-meta text-pretty">{info.message}</p> : null}
            {info.reasons.length > 0 ? (
              <ul className="mt-2 list-disc space-y-1 pl-4 type-meta">
                {info.reasons.map((reason) => <li key={reason}>{reason}</li>)}
              </ul>
            ) : null}
            {info.resources.length > 0 ? (
              <ul className="mt-2 space-y-1 font-mono text-xs" translate="no">
                {info.resources.map((resource) => (
                  <li key={`${resource.namespace}/${resource.kind}/${resource.name}`}>
                    {resource.kind}/{resource.name}
                    {resource.namespace ? ` · ${resource.namespace}` : ""}
                  </li>
                ))}
              </ul>
            ) : null}
          </div>
          {info.installable && !attempted ? (
            <form
              className="flex flex-col gap-4"
              onSubmit={(event) => { event.preventDefault(); review() }}
            >
              <label className="flex flex-col gap-1.5 text-sm">
                Installation profile
                <select
                  className="h-9 rounded-md border bg-background px-3 outline-none focus-visible:ring-2 focus-visible:ring-ring"
                  value={effectiveProfile}
                  onChange={(event) => {
                    setProfile(event.target.value as ComponentProfile)
                    setPreview(undefined)
                  }}
                >
                  {info.profiles.map((entry) => (
                    <option key={entry} value={entry}>
                      {entry === "multi-node" ? "Multiple nodes" : "Single node"}
                    </option>
                  ))}
                </select>
              </label>
              {info.inputs.filter((input) => !input.generated).map((input) => (
                <label key={input.name} className="flex flex-col gap-1.5 text-sm">
                  <span>{input.name}{input.required ? " (required)" : " (optional)"}</span>
                  {showValues ? <textarea
                    className="min-h-20 rounded-md border bg-background px-3 py-2 font-mono text-sm outline-none focus-visible:ring-2 focus-visible:ring-ring"
                    name={input.name}
                    value={values[input.name] ?? ""}
                    onChange={(event) => changed({ ...values, [input.name]: event.target.value })}
                    autoComplete="off"
                    spellCheck={false}
                    required={input.required}
                    aria-describedby={`component-input-${input.name}`}
                  /> : <input
                    className="h-9 rounded-md border bg-background px-3 font-mono text-sm outline-none focus-visible:ring-2 focus-visible:ring-ring"
                    name={input.name}
                    value={values[input.name] ?? ""}
                    onChange={(event) => changed({ ...values, [input.name]: event.target.value })}
                    type="password"
                    autoComplete="off"
                    spellCheck={false}
                    required={input.required}
                    aria-describedby={`component-input-${input.name}`}
                  />}
                  <span id={`component-input-${input.name}`} className="type-meta text-muted-foreground">
                    {input.description}
                  </span>
                </label>
              ))}
              {info.inputs.some((input) => !input.generated) ? (
                <label className="flex items-center gap-2 type-meta text-muted-foreground">
                  <input type="checkbox" checked={showValues} onChange={(event) => setShowValues(event.target.checked)} />
                  Show and edit values (supports multiple lines)
                </label>
              ) : null}
              {info.inputs.some((input) => input.generated) ? (
                <p className="type-meta text-pretty text-muted-foreground">
                  New keys are generated securely during review and stored as Kubernetes Secrets.
                  Their values are excluded from the preview.
                </p>
              ) : null}
              <div>
                <Button type="submit" variant="outline" size="sm" disabled={!canReview} loading={plan.isFetching}>
                  {preview ? "Refresh preview" : "Review installation"}
                </Button>
              </div>
            </form>
          ) : null}
        </>
      ) : null}
      {preview && !attempted && plan.isError ? <Problem error={plan.error} /> : null}
      {preview && !attempted && plan.data ? (
        <section className="flex flex-col gap-3 border-t pt-4" aria-label="Installation preview">
          <h3 className="font-medium">Review installation</h3>
          {plan.data.warnings.length > 0 ? (
            <ul className="list-disc space-y-1 pl-4 text-sm" role="note">
              {plan.data.warnings.map((warning) => <li key={warning}>{warning}</li>)}
            </ul>
          ) : null}
          <PlanDiffs writes={plan.data.writes} />
          <div className="flex flex-wrap items-center gap-3">
            <Button size="sm" disabled={!canApply} loading={apply.isPending} onClick={install}>
              Install {component.name}
            </Button>
            <span className="type-meta text-muted-foreground" role="status">
              {secondsLeft === undefined || secondsLeft <= 0
                ? "Preview expired. Refresh it before installing."
                : `Preview expires in ${clock(secondsLeft)}`}
            </span>
          </div>
        </section>
      ) : null}
      {attempted ? (
        <div className="flex flex-col gap-3 border-t pt-4">
          {apply.isError ? <Problem error={apply.error} /> : null}
          <p className="type-meta text-pretty text-muted-foreground" role="status">
            {complete
              ? "Installation completed and the component passed its health checks."
              : apply.isError
                ? "The request did not complete. Check its status before preparing another installation. It has not been retried."
                : "The installation request has been submitted. Success is reported only after the controller and component are healthy. You can close this dialog and return to check progress."}
          </p>
          <div>
            <Button variant="outline" size="sm" loading={installation.isFetching} onClick={() => void installation.refetch()}>
              Check installation status
            </Button>
          </div>
        </div>
      ) : null}
    </>
  )
}

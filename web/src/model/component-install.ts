/** Pure boundaries for the native component installer; no credentials or plan tokens are logged. */
export interface ComponentInput {
  name: string
  description: string
  required: boolean
  generated: boolean
}

export function componentInstallationPath(component: string): string {
  if (!/^[a-z0-9][a-z0-9-]*\/[a-z0-9][a-z0-9-]*$/.test(component)) {
    throw new Error("Choose a component from the catalog before installing it.")
  }
  const [capability, name] = component.split("/")
  return `/api/v1/cluster/components/${capability}/${name}/installation`
}

export function missingComponentInputs(
  inputs: ComponentInput[],
  values: Record<string, string>,
): string[] {
  return inputs
    .filter((input) => input.required && !input.generated && !values[input.name]?.trim())
    .map((input) => input.name)
}

/** Preserve significant whitespace in supplied credentials; omit unknown and generated values. */
export function providedComponentInputs(
  inputs: ComponentInput[],
  values: Record<string, string>,
): Record<string, string> {
  return Object.fromEntries(
    inputs
      .filter((input) => !input.generated && values[input.name] !== undefined && values[input.name] !== "")
      .map((input) => [input.name, values[input.name]!]),
  )
}

export function installationIsComplete(state: string): boolean {
  return state === "installed"
}

/** Only a server-issued continuation for a still-pending reviewed installation may advance. */
export function nextInstallationToken(
  info: { state: string; continuationToken?: string | null; phase?: number; totalPhases?: number; resumeAvailable?: boolean } | undefined,
  submitted: ReadonlySet<string>,
): string | undefined {
  if (!info || info.state !== "pending" || info.resumeAvailable || !info.continuationToken ||
      !Number.isInteger(info.phase) || !Number.isInteger(info.totalPhases) ||
      info.phase! < 0 || info.phase! >= info.totalPhases! || submitted.has(info.continuationToken)) {
    return undefined
  }
  return info.continuationToken
}

export function canApplyComponentPlan(
  plan: {
    component: string
    profile: string
    token: string
    expiresAt: string
    writes: unknown[]
  } | undefined,
  component: string,
  profile: string,
  now = Date.now(),
): boolean {
  return Boolean(
    plan &&
      plan.component === component &&
      plan.profile === profile &&
      plan.token &&
      plan.writes.length > 0 &&
      Date.parse(plan.expiresAt) > now,
  )
}

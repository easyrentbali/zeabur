import { zeaburFetch } from "@/api/client"
import { componentInstallationPath, type ComponentInput } from "@/model/component-install"

export type ComponentProfile = "single-node" | "multi-node"
export interface ComponentInstallation {
  component: string
  profiles: ComponentProfile[]
  inputs: ComponentInput[]
  installable: boolean
  reasons: string[]
  state: "notInstalled" | "pending" | "installed" | "failed" | "external"
  resources: { kind: string; name: string; namespace: string }[]
  message: string
}

export interface ComponentInstallRequest {
  component: string
  profile: ComponentProfile
  inputs: Record<string, string>
}

export interface ComponentInstallPlan {
  token: string
  expiresAt: string
  component: string
  profile: ComponentProfile
  writes: {
    kind: string
    name: string
    namespace: string
    beforeYaml: string | null
    afterYaml: string
  }[]
  warnings: string[]
}

export interface ComponentInstallSubmission {
  component: string
  state: "submitted"
  written: string[]
}

export function getComponentInstallation(component: string, signal?: AbortSignal) {
  return zeaburFetch<ComponentInstallation>(componentInstallationPath(component), { signal })
}

/** A preview does not write resources. Input values never appear in a URL or query key. */
export function planComponentInstall(request: ComponentInstallRequest, signal?: AbortSignal) {
  return zeaburFetch<ComponentInstallPlan>("/api/v1/cluster/components/install/plan", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(request),
    signal,
  })
}

/** Submit once; a missing answer requires checking installation status rather than retrying. */
export function applyComponentInstall(token: string) {
  return zeaburFetch<ComponentInstallSubmission>("/api/v1/cluster/components/install/apply", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ token }),
  })
}

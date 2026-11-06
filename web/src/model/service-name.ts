/** A display title never changes the workload identity used by links and writes. */
export function serviceTitle(service: { name: string; displayName?: string | null }): string {
  return service.displayName?.trim() || service.name
}

import { expect, test } from "bun:test"
import { serviceTitle } from "./service-name"

test("a readable title preserves the service identity used by links and writes", () => {
  const service = { name: "service-6a962b55a5e5232732f45479", displayName: "Waxum Gateway" }
  expect(serviceTitle(service)).toBe("Waxum Gateway")
  expect(service.name).toBe("service-6a962b55a5e5232732f45479")
})

test("missing and blank titles retain the Kubernetes name", () => {
  expect(serviceTitle({ name: "service-existing" })).toBe("service-existing")
  expect(serviceTitle({ name: "service-existing", displayName: null })).toBe("service-existing")
  expect(serviceTitle({ name: "service-existing", displayName: "  " })).toBe("service-existing")
})

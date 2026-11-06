---
default: patch
---

# Components show runtime versions from Pod template labels

Controllers such as System Upgrade Controller now show their configured image version when their identifying labels exist only on the Pod template. Workload labels retain precedence, and runtime versions remain separate from chart comparisons.

The catalog also recognizes the existing Zeabur Operator VictoriaLogs backend by its exact StatefulSet identity and official image. This reports backend presence without certifying log collection or creating another store.

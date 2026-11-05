---
default: patch
---

# Preserve ERB dashboard installation with catalog v3

The dashboard installer now reads typed generated inputs, Pod Security levels and contribution release targets from the catalog used by v0.2.0-beta.1. Inputs without an explicit release apply to the first chart after any manifest prerequisites. Unknown release types, security levels and generators remain refused before installation.

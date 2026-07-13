# Security Policy

factor-q is alpha software. This page is the canonical summary of its
current security posture; [STATUS.md](STATUS.md) retains the operational
one-line caveats.

## Current posture

- **Sandbox:** built-in tools are denied by default, and filesystem and
  command working-directory path allowlists are enforced. Agent definitions
  may also declare `sandbox.env` and `sandbox.network`, but those declarations
  are not yet enforced ([#34](https://github.com/bricef/factor-q/issues/34),
  [#35](https://github.com/bricef/factor-q/issues/35)). Until they are, treat
  every agent as network-unrestricted regardless of its definition.
- **NATS:** the bundled NATS service has no authentication. Do not expose its
  port beyond the host.
- **`fq-cas serve`:** the content-store service is localhost-only and
  unauthenticated until M5.
- **Agent identity:** agent GitHub actions currently use the owner's
  `GH_TOKEN`; per-agent identity and attestation are still
  [design work](docs/design/aspirational/agent-identity-and-attestation.md).

## Reporting a Vulnerability

Whilst this project is in alpha (version < 1.0.0), please raise security
issues as normal GitHub issues.

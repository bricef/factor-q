# Security Policy

factor-q is alpha software. This page is the canonical summary of its
current security posture; [STATUS.md](STATUS.md) retains the operational
one-line caveats.

## Current posture

- **Sandbox:** built-in tools are denied by default, and filesystem and
  command working-directory path allowlists are enforced. Agent definitions
  may also declare `sandbox.env` and `sandbox.network`, but those declarations
  are not yet enforced. Until they are, treat every agent as
  network-unrestricted regardless of its definition. Enforcement is tracked
  by [#208](https://github.com/bricef/factor-q/issues/208) (a CONNECT-filtering
  forward proxy) and [#209](https://github.com/bricef/factor-q/issues/209)
  (containerised isolation, ADR-0010); the issues that first identified the
  gap, [#34](https://github.com/bricef/factor-q/issues/34) and
  [#35](https://github.com/bricef/factor-q/issues/35), are closed — #35 by a
  change that added a load-time warning, not enforcement.
- **NATS:** the bundled NATS service requires a static development token. The
  token is committed to this public repository, so it is not a secret: do not
  expose the port beyond the host, and replace the token for any non-local
  deployment.
- **`fq-cas serve`:** the content-store service is localhost-only and
  unauthenticated until M5.
- **Agent identity:** agent GitHub actions currently use the owner's
  `GH_TOKEN`; per-agent identity and attestation are still
  [design work](docs/design/aspirational/agent-identity-and-attestation.md).
- **Dependencies:** every push and pull request runs `just audit` —
  `cargo audit` and `cargo deny` against `deny.toml`, the reviewed advisory
  and licence baseline (one explained ignore per accepted finding, never a
  blanket allow). Dependabot opens weekly update PRs for the Cargo
  workspace, the Go adapters and the workflow actions. The `main-latest`
  binaries the dogfood host pulls are built from a lockfile that has
  passed this gate.

## Reporting a Vulnerability

Whilst this project is in alpha (version < 1.0.0), please raise security
issues as normal GitHub issues.

# Security Policy

factor-q is alpha software. This page is the canonical summary of its
current security posture; [STATUS.md](STATUS.md) retains the operational
one-line caveats.

## Current posture

- **Sandbox:** built-in tools are denied by default, and filesystem and
  command working-directory path allowlists are enforced. Agent definitions
  may also declare `sandbox.env` and `sandbox.network`; the environment
  allowlist limits child inheritance and Linux makes direct reads of
  `/proc/<fqd-pid>/environ` root-only, but arbitrary exec can still reach
  secrets by other means, while network declarations are not yet enforced.
  Treat every agent as network-unrestricted regardless of its definition.
  The sandbox dimensions are therefore not independent: an `exec_cwd` grant
  lets a process read and write anything its OS identity can access, inspect
  ambient secrets (including through `/proc`), and make unrestricted network
  requests, regardless of narrower `fs_read`, `fs_write`, `env`, or `network`
  declarations. In practice, exec dominates the other grants; they constrain
  the built-in tools, not programs started by exec. Do not use a narrow
  filesystem or environment declaration alongside exec as an isolation
  boundary.
  Enforcement is tracked
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
  unauthenticated until M5. Its RPC surface is restricted to `put`, `get`,
  `get_range`, `has`, `size`, and `stats`; GC-only removal and block-inspection
  operations run in-process and are not exposed over the wire.
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

## Fleet residual risk

The dogfood fleet's unattended agents process untrusted public issue text with
the repository owner's ambient, write-scoped `GH_TOKEN` and can reach other
owner credentials, including model API secrets, through exec. Their network
declarations are not
enforced, and an agent with `builtin__exec` can deliberately invoke a shell
(for example, `bash -c`) despite the tool's argv-only interface. Restrictions
against operations such as pushing or merging are prompt instructions, not
typed or runtime-enforced controls. Public issue text can therefore influence
the backlog groomer, which rewrites specifications consumed by an implementing
agent.

For that chain, the **human review and merge gate is the sole enforced
control** before agent-produced changes reach the protected branch. Treat the
fleet as privileged automation with attacker-controlled input, not as sandboxed
execution, until stronger credential, network, and process isolation ships.

## Reporting a Vulnerability

Whilst this project is in alpha (version < 1.0.0), please raise security
issues as normal GitHub issues.

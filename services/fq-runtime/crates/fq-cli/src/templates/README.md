# factor-q project

This directory was initialised with `fq init`. It contains:

- `fqd.toml` — the daemon's configuration: broker, agents directory,
  model registry, retention
- `fq.toml` — the client's configuration: which daemon `fq` talks to
- `docker-compose.yml` — NATS (with JetStream) for local development
- `agents/` — agent definitions (Markdown with YAML frontmatter)
- `agents/sample-agent.md` — a minimal working agent to test the pipeline

Two binaries, so two config files, each named for the one that reads
it. `fqd` is the daemon: it owns the broker connection, the stores and
every invocation. `fq` is the client: it asks a daemon questions over
an authenticated connection, and works just as well against a daemon on
another machine — which is why it has no use for `fqd.toml`.

## Prerequisites

1. **NATS with JetStream** — factor-q publishes all events through a
   NATS server and expects JetStream to be enabled. A ready-to-use
   `docker-compose.yml` is included; start it with `docker compose up -d`.
2. **LLM provider API key** — export the key for any provider your
   agents target, for example:
   ```sh
   export ANTHROPIC_API_KEY='sk-ant-...'
   ```

See the [deployment guide][deployment] for full setup details.

## Quick start

```sh
# Start NATS (JetStream) in the background
docker compose up -d

# Validate the sample agent definition. This one needs no daemon —
# linting a file before it is deployed is an offline operation.
fq agent validate agents/sample-agent.md

# The daemon reads the broker token from the environment variable
# fqd.toml names (`[nats] token_env`, FQ_NATS_TOKEN) — never from the
# URL, which it prints. The scaffolded compose file starts NATS with
# the development token:
export FQ_NATS_TOKEN=fq-dev-token

# Start the daemon from this directory, so it reads fqd.toml. On its
# first run it prints a certificate fingerprint and an admin token,
# once — keep the token.
fqd
```

Then, in another shell, pair the client with it. The edge listens on
`127.0.0.1:9472` unless `fqd.toml`'s `[edge] bind` says otherwise, and
the first connection asks you to confirm the fingerprint the daemon
printed:

```sh
fq connect 127.0.0.1:9472 --token <token>

# List the agents the daemon loaded
fq agent list

# Trigger the sample agent with a message
fq trigger sample-agent "Say hello in one sentence."

# Watch the run
fq events tail
```

The pairing is stored once, so later `fq` commands need neither the
address nor the token. Set `[daemon] addr` in `fq.toml` only when you
have paired with more than one daemon and want a default.

## Next steps

- Edit `agents/sample-agent.md` or add new agent files under `agents/`.
  `fq reload` makes the daemon re-read them without a restart.
- Override any of the daemon's configuration when starting `fqd`, with
  flags (`--agents-dir`, `--nats-url`, `--cache-dir`, `--state-dir`) or
  environment variables (`FQ_AGENTS_DIR`, `FQ_NATS_URL`,
  `FQ_CACHE_DIR`, `FQ_STATE_DIR`). These configure the runtime, so they
  belong to the daemon; `fq` does not accept them.
- Point `fq` at a particular daemon with `--addr` (or `FQ_ADDR`), and
  at a different client config with `--config` (or `FQ_CLI_CONFIG`).

## Documentation

- [Project documentation][main]
- [Deployment guide][deployment]
- [Agent definition format (ADR-0005)][adr-0005]

[main]: https://github.com/bricef/factor-q
[deployment]: https://github.com/bricef/factor-q/blob/main/services/fq-runtime/README.md
[adr-0005]: https://github.com/bricef/factor-q/blob/main/docs/adrs/accepted/0005-agent-definition-format.md

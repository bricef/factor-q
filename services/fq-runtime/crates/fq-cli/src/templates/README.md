# factor-q project

This directory was initialised with `fq init`. It contains:

- `fq.toml` — factor-q runtime configuration
- `agents/` — agent definitions (Markdown with YAML frontmatter)
- `agents/sample-agent.md` — a minimal working agent to test the pipeline

## Prerequisites

1. **NATS with JetStream** — factor-q publishes all events through a
   NATS server and expects JetStream to be enabled.
2. **LLM provider API key** — export the key for any provider your
   agents target, for example:
   ```sh
   export ANTHROPIC_API_KEY='sk-ant-...'
   ```

See the [deployment guide][deployment] for full setup details including
running NATS locally with Docker Compose.

## Quick start

```sh
# List the agents this project defines
fq agent list

# Validate a specific agent definition
fq agent validate agents/sample-agent.md

# Trigger the sample agent with a message
fq trigger sample-agent "Say hello in one sentence."

# In another terminal, tail the event stream
fq events tail
```

## Next steps

- Edit `agents/sample-agent.md` or add new agent files under `agents/`.
- Override any configuration field from `fq.toml` using CLI flags
  (`--agents-dir`, `--nats-url`, `--cache-dir`) or environment variables
  (`FQ_AGENTS_DIR`, `FQ_NATS_URL`, `FQ_CACHE_DIR`).

## Documentation

- [Project documentation][main]
- [Deployment guide][deployment]
- [Agent definition format (ADR-0005)][adr-0005]

[main]: https://github.com/bricef/factor-q
[deployment]: https://github.com/bricef/factor-q/blob/main/services/fq-runtime/README.md
[adr-0005]: https://github.com/bricef/factor-q/blob/main/docs/adrs/accepted/0005-agent-definition-format.md

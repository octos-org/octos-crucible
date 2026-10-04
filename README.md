# octos-crucible

A general platform for evaluating agents: any agent that runs in a sandbox, on any taskset, run stage by stage and scored, with wall time, tokens, cache hit rate and equivalent cost recorded objectively. Writing code is just one kind of task:

- **Coding**: ARC-Bench GitHub tasks (`tasksets/arcbench-github`), the agent builds a website, scored by Playwright tests.
- **Interactive decision-making**: GOSIM Agentic Observer (`tasksets/astro-practice`), an observing agent answers a simulator turn by turn, scored by the official engine ([docs/astro-survey.md](docs/astro-survey.md)).
- **Math / reasoning** (coming soon): e.g. IMO problems, the agent writes proofs and a judge model scores them against a rubric; model-based scoring is planned in [docs/plugins.md](docs/plugins.md).

Tasksets, runners, packagers and scorers are pluggable ([docs/plugins.md](docs/plugins.md)).

Design: [docs/DESIGN.md](docs/DESIGN.md) · Agent packages: [docs/agent-contract.md](docs/agent-contract.md) · Self-hosted runners: [docs/self-hosted.md](docs/self-hosted.md)

## Layout

| crate | role |
|---|---|
| `crucible-core` | shared file formats (taskset, agent.json, usage records, result.json, manifest, envelope); no IO, builds for wasm32 |
| `crucible-metering` | usage parsing, price lookup, equivalent cost |
| `crucible-meter` | metering proxy (`crucible meter`) |
| `crucible-egress` | CONNECT allowlist proxy (`crucible egress`) |
| `crucible-crypto` | age X25519 envelopes, AES-256 password zips |
| `crucible-store` | content-addressed blob store (local dir, GitHub Releases) |
| `crucible-report` | usage pricing, multi-replica / multi-stage statistics |
| `crucible-cli` | the `crucible` binary |

Other directories: `agents/` (builtin agent packages, same format as uploads), `tasksets/<name>/` (`source.json` = how a source tree is cut into sealed inputs/tests blobs; `taskset.json` = the registered result of `crucible taskset pack`), `tools/sandbox-net.sh` (agent network sandbox on the runner), `.github/workflows/eval.yml` (generation: setup → generate × replicas → publish).

Config: `config/pricing.json` (public prices, USD per 1M tokens), `config/egress.json` (package registries the agent may reach).

## Build and test

```sh
cargo build --release
cargo test --workspace
```

## 使用指南

给平台使用者的中文文档：[docs/guide/README.md](docs/guide/README.md)

路线图：[docs/ROADMAP.md](docs/ROADMAP.md)

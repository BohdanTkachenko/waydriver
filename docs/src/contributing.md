# Contributing Guide

The canonical guidance for working in this repository — development environment,
build/test commands, the architecture deep-dive, testing notes, the CI pipeline,
and commit-message conventions — lives in
[`AGENTS.md`](https://github.com/BohdanTkachenko/waydriver/blob/main/AGENTS.md)
at the repo root. It is written for both human contributors and AI coding
assistants. This page covers the one thing most contributors hit first: building
without Nix.

## Developing without Nix

Contributors who don't use Nix can build and test the workspace directly once the
[system packages](./getting-started.md#requirements) are installed. Two repo
helpers automate this:

- [`.claude/hooks/session-start.sh`](https://github.com/BohdanTkachenko/waydriver/blob/main/.claude/hooks/session-start.sh)
  installs the build + runtime packages, ensures the `rustfmt`/`clippy` rustup
  components, and warms the crate cache. It is gated on `$CLAUDE_CODE_REMOTE`, so
  it only runs in the Claude Code cloud env; on another machine, run the
  apt/dnf/pacman command for your distro instead.
- [`scripts/dev-container.sh`](https://github.com/BohdanTkachenko/waydriver/blob/main/scripts/dev-container.sh)
  drops you into a Fedora 42 shell (matching the Dockerfile/CI) with your working
  tree bind-mounted, for building `waydriver-fixture-gtk` and running the native
  e2e suite. These need libadwaita ≥ 1.6, so they can't build on Ubuntu 24.04
  (which ships 1.5).

On a non-Nix host, build and test the rest of the workspace with
`--exclude waydriver-fixture-gtk`, and set `GST_PLUGIN_PATH`, `XDG_DATA_DIRS`, and
the `at-spi2-core/libexec` path yourself when running the raw binary (the
`nix run .#mcp` wrapper that injects these is Nix-only). See
[`AGENTS.md`](https://github.com/BohdanTkachenko/waydriver/blob/main/AGENTS.md)
for details.

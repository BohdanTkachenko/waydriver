# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [0.2.1](https://github.com/BohdanTkachenko/waydriver/compare/waydriver-v0.2.0...waydriver-v0.2.1) - 2026-04-26

### Added

- *(locator)* pointer-click fallback when fill target lacks Component::grab_focus
- *(input)* thread CancellationToken through InputBackend for prompt kill
- *(locator)* element-scoped pointer actions (hover, double_click, right_click, drag_to)
- *(locator)* Locator::select_option via AT-SPI Selection interface
- *(locator)* layered wait_for / wait_until / wait_until_async primitives
- *(input)* Locator::scroll_into_view with AT-SPI + wheel fallbacks
- *(input)* Locator::fill(), absolute pointer motion, Session::type_text (WAY-5)
- *(atspi)* capture element bounds via Component::get_extents
- *(locator)* add richer AT-SPI state predicates and matching waiters

### Fixed

- *(session)* bound kill latency with AT-SPI method timeout and shutdown budget
- *(mcp)* kill_session no longer blocks on in-flight tool auto-waits

### Other

- refresh AGENTS.md and README.md for current API surface
- workspace-wide audit pass tightening trait surfaces and error types
- *(error)* preserve typed error sources on Atspi/Process/Screenshot
- split e2e tests into waydriver-e2e crate, add configurable video_fps
- *(mcp)* split tool handlers into per-concern modules
- *(mcp)* split monolithic main.rs into focused modules
- *(compositor-mutter)* separate doc paragraph before stage rationale

## [0.2.0](https://github.com/BohdanTkachenko/waydriver/compare/waydriver-v0.1.3...waydriver-v0.2.0) - 2026-04-24

### Added

- *(fixture)* GTK4/libadwaita e2e fixture with stdout event capture
- *(input)* keyboard chord support via key_down/key_up primitives
- *(atspi)* Locator::focus via Component::grab_focus
- *(atspi)* auto-wait and explicit wait_for_* on Locator
- *(atspi)* [**breaking**] XPath-based locator API over AT-SPI tree
- *(capture)* WebM video recording for sessions
- *(mcp)* configurable virtual-monitor resolution
- *(mcp)* per-session event log and static HTML viewer
- *(mcp)* configurable report dir with per-session screenshot counter

### Other

- update README and AGENTS.md for Locator API
- *(release)* move CHANGELOG.md into waydriver crate with root symlink
- *(release)* consolidate per-crate changelogs into workspace CHANGELOG
- *(mcp)* drop flaky second-screenshot assertion in e2e

## [0.1.3](https://github.com/BohdanTkachenko/waydriver/compare/waydriver-v0.1.2...waydriver-v0.1.3) - 2026-04-17

### Added

- add publishable builder image and document multi-language dev workflows

## [0.1.2](https://github.com/BohdanTkachenko/waydriver/compare/waydriver-v0.1.1...waydriver-v0.1.2) - 2026-04-16

### Added

- *(mcp)* add Docker packaging and container-based e2e test

## [0.1.1](https://github.com/BohdanTkachenko/waydriver/compare/waydriver-v0.1.0...waydriver-v0.1.1) - 2026-04-16

### Added

- add MCP server for AI-driven headless UI testing

### Other

- add rustdoc comments to public API surface
- add per-distro dependency tables and install commands to README

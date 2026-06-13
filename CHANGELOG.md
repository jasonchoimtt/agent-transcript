# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2](https://github.com/jasonchoimtt/agent-transcript/compare/v0.1.1...v0.1.2) - 2026-06-13

### Other

- add demo gif
- add version badges
- update README about installer and new features

## [0.1.1](https://github.com/jasonchoimtt/agent-transcript/compare/v0.1.0...v0.1.1) - 2026-06-13

### Added

- add JsonlReader to buffer out-of-order JSONL entries
- mode routing, focus events, hover gutter, and prompt overlay improvements
- pinned/floating prompt box overlay
- mouse hover and click integration for transcript tree
- improve turn-end navigation with same-type run-start retreat
- add vim-style marks and jump list
- add incremental search in transcript view
- extend ToolResultEnricher to support Cursor provider
- expanded flag on tool formatter rules and tool grouper groups
- hidden message navigation (J/K/zJ/zK/o/c/O/C/zh)
- tool result widget with file delta and shell output rendering
- add ! composite key prefix for debug/info bindings

### Fixed

- show new workspace path at picker
- incorrect implementation of [] movement
- restore key shortcuts view to ':'
- enrich claude fresh Write, fix hunk tokenization logic
- fix selection after each op to avoid selection jump issue
- hide attachment messages, collapse summary by default
- per-change-block full-diff decision in build_diff_lines
- disallow unsetting show_more when nothing to show less
- claude path should not drop root message

### Other

- setup cargo-dist
- add cargo attributes
- fix clippy errors
- fix install command
- batch consecutive scroll events
- notify component width changed lazily
- update legacy reexport
- reorganize message_widget using MessageComponent trait
- organize mode-related flags into a single AppMode enum
- move table and tool_result under message_widget
- split message_widget into sub-modules and embed MessageState
- document tool formatter and grouper
- remove per-hunk pagination; compact mode shows up to 3 hunks

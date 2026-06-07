#!/usr/bin/env python3
"""Render a terminal fixture file to readable text using a vt100 parser.

Builds a small Rust binary on first use (cached in /tmp/fixture_renderer).
The binary is rebuilt automatically if the source changes.

Usage:
  render_fixture.py <fixture.txt> [rows=40] [cols=auto]

If cols is omitted, the native capture width is auto-detected from the
background-color fill sequences in the file.

Examples:
  render_fixture.py src/terminal/fixtures/screenshots/claude/idle.txt
  render_fixture.py src/terminal/fixtures/screenshots/cursor/approval.txt 20 93
"""
import sys
import os
import re
import subprocess

RENDERER_DIR = "/tmp/fixture_renderer"
RENDERER_BIN = f"{RENDERER_DIR}/target/debug/fixture_renderer"

CARGO_TOML = """\
[package]
name = "fixture_renderer"
version = "0.1.0"
edition = "2024"

[dependencies]
vt100 = "0.15"
"""

MAIN_RS = r"""
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("Usage: fixture_renderer <path> [rows] [cols]");
    let rows: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(40);
    let cols: u16 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(200);
    let bytes = std::fs::read(path).expect("read fixture");
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(&bytes);
    let screen = parser.screen();
    for r in 0..rows {
        let text: String = (0..cols)
            .filter_map(|c| screen.cell(r, c))
            .map(|cell| {
                let s = cell.contents();
                if s.is_empty() { ' ' } else { s.chars().next().unwrap_or(' ') }
            })
            .collect();
        let trimmed = text.trim_end();
        if !trimmed.is_empty() {
            println!("row {:2}: {}", r, trimmed);
        }
    }
}
"""


def detect_native_width(path: str) -> int | None:
    with open(path, "rb") as f:
        data = f.read().decode("latin-1")
    runs = re.findall(r"\x1b\[48;5;\d+m( +)", data)
    if not runs:
        return None
    return max(len(r) for r in runs) + 1


def build_renderer():
    src_path = f"{RENDERER_DIR}/src/main.rs"
    needs_build = not os.path.exists(RENDERER_BIN)

    if not needs_build:
        # Rebuild if source changed.
        current = open(src_path).read() if os.path.exists(src_path) else ""
        needs_build = current.strip() != MAIN_RS.strip()

    if needs_build:
        os.makedirs(f"{RENDERER_DIR}/src", exist_ok=True)
        with open(f"{RENDERER_DIR}/Cargo.toml", "w") as f:
            f.write(CARGO_TOML)
        with open(src_path, "w") as f:
            f.write(MAIN_RS)
        print("Building vt100 renderer (first run only)…", file=sys.stderr)
        subprocess.run(["cargo", "build"], cwd=RENDERER_DIR, check=True,
                       capture_output=False)


def render(fixture: str, rows: str = "40", cols: str | None = None):
    if cols is None:
        native = detect_native_width(fixture)
        cols = str(native) if native else "200"

    build_renderer()
    subprocess.run([RENDERER_BIN, fixture, rows, cols], check=True)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)

    fixture = sys.argv[1]
    rows = sys.argv[2] if len(sys.argv) > 2 else "40"
    cols = sys.argv[3] if len(sys.argv) > 3 else None
    render(fixture, rows, cols)

#!/usr/bin/env python3
"""Detect the native terminal width of one or more fixture files.

Terminal fixtures are captured at a specific width. Rendering at a
different width causes text wrapping that distorts layout and hides
content. This script infers the capture width from the longest
background-color space-fill sequence in the raw escape stream.

Usage:
  detect_native_width.py <fixture.txt> [<fixture.txt> ...]
"""
import sys
import re


def detect(path: str) -> int | None:
    with open(path, "rb") as f:
        data = f.read().decode("latin-1")

    # Heuristic 1 (Cursor-style): ESC[48;5;NNNm followed by spaces fills a
    # row to the terminal edge. One leading uncolored space comes before the
    # fill, so native_width = longest_run + 1.
    runs = re.findall(r"\x1b\[48;5;\d+m( +)", data)
    if runs:
        return max(len(r) for r in runs) + 1

    # Heuristic 2 (Claude-style): the input-box divider is a run of U+2500
    # (─, UTF-8: \xe2\x94\x80) that exactly fills the terminal width.
    divider = b"\xe2\x94\x80"
    with open(path, "rb") as f:
        raw = f.read()
    runs2 = re.findall(rb"(?:" + re.escape(divider) + rb")+", raw)
    if runs2:
        longest = max(len(r) // len(divider) for r in runs2)
        if longest >= 10:
            return longest
    return None


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)

    for path in sys.argv[1:]:
        width = detect(path)
        if width:
            print(f"{path}: {width} cols")
        else:
            print(f"{path}: no bg fill detected — use default width (200)")

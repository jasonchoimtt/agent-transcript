---
name: redacting-screenshots
description: Review terminal fixture screenshots for sensitive information and redact it in-place. Use when adding new screenshot fixtures under src/terminal/fixtures/screenshots/ or auditing existing ones.
---

# Redacting terminal fixture screenshots

Fixture files are raw PTY output containing ANSI escape sequences. **Never read them directly** — the escape sequences will corrupt the Claude session. Always use the renderer script.

## Workflow

1. **Detect native widths** — run `detect_native_width.py` on each fixture. Rendering at the wrong width causes text to wrap and hides or distorts content.
2. **Render all fixtures** — run `render_fixture.py` to produce readable text from each fixture via a proper vt100 parser.
3. **Identify sensitive content** — look for: session UUIDs, project/directory names, branch names and git hashes, usage percentages and time remaining, dates/times/timezones, API keys or tokens, usernames, IP addresses.
4. **Plan replacements** — each replacement must have the exact same byte length as the original, or the terminal layout breaks (escape sequence offsets shift and the screen renders incorrectly).
5. **Check for interleaving** — some visible strings are not stored as contiguous bytes; use `redact.py --find <string>` to check before replacing.
6. **Apply redactions** — run `redact.py` with your substitution pairs.
7. **Verify** — re-render the modified fixtures, then run `cargo test` to confirm the crop detector tests still pass.

## Detecting native widths

```
python3 .claude/skills/redacting-screenshots/scripts/detect_native_width.py \
  src/terminal/fixtures/screenshots/claude/*.txt \
  src/terminal/fixtures/screenshots/cursor/*.txt
```

The script finds the longest background-color space fill (`\x1b[48;5;NNNm` + spaces) to infer the terminal width at capture time.

## Rendering fixtures

```
python3 .claude/skills/redacting-screenshots/scripts/render_fixture.py <fixture.txt> [rows] [cols]
```

Omit `cols` to use the auto-detected native width. `rows` defaults to 40. The vt100 Rust renderer is built on first use and cached in `/tmp/fixture_renderer`.

To render all fixtures at once:
```
for f in src/terminal/fixtures/screenshots/**/*.txt; do
  echo "=== $f ==="
  python3 .claude/skills/redacting-screenshots/scripts/render_fixture.py "$f"
done
```

## Designing replacements

Replacements must preserve exact byte length. Use same-length placeholders.

Verify byte lengths before applying:
```
python3 .claude/skills/redacting-screenshots/scripts/redact.py \
  --fixture <path> \
  --replace "abcde" "myprj" \
  --replace "73bb5098" "00000000" \
  --dry-run
```

## Interleaved escape sequences

Status bar text in Claude Code and Cursor is rendered by a TUI with color codes between tokens. The spaces between words are often `\x1b[C` (cursor-forward) commands or non-breaking spaces (`\xc2\xa0`) with color resets around them — not plain ASCII spaces. Simple string replacement silently fails in this case.

Diagnose with:
```
python3 .claude/skills/redacting-screenshots/scripts/redact.py --fixture <path> --find "May 5, 8pm"
```

If the output shows the string split by `\x1b[C`, replace each word token separately:
```
--replace "May" "XXX" --replace "5," "XX" --replace "8pm" "XXX"
```

If the surrounding escape bytes are needed as a context anchor (e.g. `5m` that could collide with SGR code `\x1b[5m`), use raw byte patterns in the script directly.

## Applying redactions

```
python3 .claude/skills/redacting-screenshots/scripts/redact.py \
  --fixture src/terminal/fixtures/screenshots/claude/idle.txt \
  --replace "abcde" "myprj" \
  --replace "73bb5098" "00000000" \
  --replace "May" "XXX" \
  --replace "5," "XX" \
  --replace "8pm" "XXX"
```

To apply the same set across multiple files, loop:
```
for f in src/terminal/fixtures/screenshots/claude/*.txt; do
  python3 .claude/skills/redacting-screenshots/scripts/redact.py \
    --fixture "$f" --replace "abcde" "myprj" --replace "73bb5098" "00000000"
done
```

The script refuses to apply any pair where byte lengths differ, prints a warning for strings not found, and reports how many times each replacement was made.

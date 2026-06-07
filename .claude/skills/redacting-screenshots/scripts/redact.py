#!/usr/bin/env python3
"""Apply byte-level redactions to terminal fixture files.

Replacements MUST preserve exact byte length — different-length substitutions
shift all subsequent escape sequence offsets and corrupt the terminal layout.

The --find flag is useful for diagnosing strings that appear visually in the
rendered output but cannot be found as contiguous bytes (e.g. status-bar text
where spaces are stored as ESC[C cursor-forward commands or non-breaking spaces
with surrounding color resets). In that case, replace each word token separately.

Usage:
  redact.py --fixture <path> --replace <old> <new> [--replace <old> <new> ...]
  redact.py --fixture <path> --find <string>
  redact.py --fixture <path> --replace <old> <new> --dry-run

Examples:
  # Replace project name and git hash everywhere in a fixture
  redact.py --fixture src/terminal/fixtures/screenshots/claude/idle.txt \\
    --replace "abcde" "myprj" --replace "73bb5098" "00000000"

  # Check if a string is stored as contiguous bytes
  redact.py --fixture src/terminal/fixtures/screenshots/claude/idle.txt \\
    --find "May 5, 8pm"

  # Apply to all fixtures in a directory
  for f in src/terminal/fixtures/screenshots/claude/*.txt; do
    redact.py --fixture "$f" --replace "abcde" "myprj"
  done
"""
import sys
import argparse


def find_in_fixture(data: bytes, needle: str) -> None:
    nb = needle.encode()
    idx = data.find(nb)
    if idx >= 0:
        ctx = data[max(0, idx - 5) : idx + len(nb) + 30]
        print(f"Found at byte offset {idx}:")
        print(f"  {ctx!r}")
    else:
        print(f"NOT FOUND as contiguous bytes: {needle!r}")
        print("The string may be interleaved with escape sequences.")
        print("Try searching for individual words/tokens:")
        for word in needle.split():
            wb = word.encode()
            wi = data.find(wb)
            if wi >= 0:
                ctx = data[max(0, wi - 3) : wi + len(wb) + 20]
                print(f"  {word!r} at {wi}: {ctx!r}")
            else:
                print(f"  {word!r}: not found")


def apply_replacements(
    data: bytes, pairs: list[tuple[str, str]], dry_run: bool
) -> bytes:
    # Validate all lengths before touching the file.
    errors = []
    for old, new in pairs:
        ob, nb = old.encode(), new.encode()
        if len(ob) != len(nb):
            errors.append(
                f"  MISMATCH {old!r} ({len(ob)}B) -> {new!r} ({len(nb)}B)"
            )
        else:
            print(f"  OK {len(ob)}B  {old!r} -> {new!r}")
    if errors:
        print("\nERROR: byte-length mismatches (fix before applying):")
        for e in errors:
            print(e)
        sys.exit(1)

    if dry_run:
        print("\n(dry run — no changes written)")
        return data

    for old, new in pairs:
        ob, nb = old.encode(), new.encode()
        count = data.count(ob)
        if count:
            data = data.replace(ob, nb)
            print(f"  replaced {count}x  {old!r} -> {new!r}")
        else:
            print(f"  WARNING not found: {old!r}")
    return data


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--fixture", required=True, help="Fixture file to modify")
    parser.add_argument(
        "--replace",
        nargs=2,
        action="append",
        metavar=("OLD", "NEW"),
        default=[],
        help="Replacement pair (repeatable)",
    )
    parser.add_argument(
        "--find",
        metavar="STRING",
        help="Show raw byte context for STRING instead of replacing",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Validate lengths and print plan without writing",
    )
    args = parser.parse_args()

    with open(args.fixture, "rb") as f:
        data = f.read()

    if args.find:
        find_in_fixture(data, args.find)
        return

    if not args.replace:
        parser.error("Provide at least one --replace pair (or use --find)")

    original = data
    data = apply_replacements(data, args.replace, args.dry_run)

    if not args.dry_run and data != original:
        with open(args.fixture, "wb") as f:
            f.write(data)
        print(f"\nWritten: {args.fixture}")
    elif not args.dry_run:
        print(f"\nNo changes: {args.fixture}")


if __name__ == "__main__":
    main()

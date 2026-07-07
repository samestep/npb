"""npd command-line entry point.

The subcommand surface is sketched here to fix the shape; each is implemented
spine-first per DESIGN.md §9. Unimplemented subcommands fail loudly rather than
pretending to work.
"""

from __future__ import annotations

import argparse
import sys


def _todo(name: str) -> None:
    raise SystemExit(f"npd {name}: not implemented yet (see DESIGN.md build order)")


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="npd", description=__doc__)
    sub = p.add_subparsers(dest="command", required=True)

    e = sub.add_parser("eval", help="evaluate a revision -> attr->drv map (cached)")
    e.add_argument("commit")
    e.add_argument("--system", action="append", default=[])

    d = sub.add_parser("diff", help="diff two revisions (optionally three-way via merge base)")
    d.add_argument("base")
    d.add_argument("head")
    d.add_argument("--three-way", action="store_true", help="also eval the merge base")

    b = sub.add_parser("build", help="build derivations, consulting the observation log")
    b.add_argument("attrs", nargs="*")
    b.add_argument("--recheck", action="store_true", help="rebuild even a known success")
    b.add_argument("--retry", action="store_true", help="re-attempt a known failure")
    b.add_argument("--prefer-local", action="store_true", help="don't trust substituted successes")

    sub.add_parser("report", help="render a Markdown report from stored facts")

    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv if argv is not None else sys.argv[1:])
    _todo(args.command)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

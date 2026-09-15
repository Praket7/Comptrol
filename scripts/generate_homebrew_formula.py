#!/usr/bin/env python3
"""Generate a Homebrew formula from a verified release archive."""

from argparse import ArgumentParser
from pathlib import Path


def main() -> int:
    parser = ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--url", required=True)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--formula-name", default="Comptrolling")
    parser.add_argument("--arm64-only", action="store_true")
    args = parser.parse_args()
    architecture = '  depends_on arch: :arm64\n' if args.arm64_only else ''
    formula = f'''class {args.formula_name} < Formula
  desc "Local first computer control runtime"
  homepage "https://github.com/Praket7/Comptrol"
  url "{args.url}"
  sha256 "{args.sha256}"
  version "{args.version}"
  license "Apache-2.0"
{architecture}

  def install
    bin.install "comptrol/comptrol"
  end

  test do
    assert_match version.to_s, shell_output("#{{bin}}/comptrol version")
  end
end
'''
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(formula, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Generate a Homebrew formula from a verified release archive."""

from argparse import ArgumentParser
from pathlib import Path


def main() -> int:
    parser = ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--url")
    parser.add_argument("--sha256")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--formula-name", default="Comptrolling")
    parser.add_argument("--arm64-only", action="store_true")
    parser.add_argument("--macos-arm64-url")
    parser.add_argument("--macos-arm64-sha256")
    parser.add_argument("--macos-intel-url")
    parser.add_argument("--macos-intel-sha256")
    parser.add_argument("--linux-arm64-url")
    parser.add_argument("--linux-arm64-sha256")
    parser.add_argument("--linux-intel-url")
    parser.add_argument("--linux-intel-sha256")
    args = parser.parse_args()
    mac_platform = all(
        getattr(args, field)
        for field in (
            "macos_arm64_url",
            "macos_arm64_sha256",
            "macos_intel_url",
            "macos_intel_sha256",
        )
    )
    linux_platform = all(
        getattr(args, field)
        for field in (
            "linux_arm64_url",
            "linux_arm64_sha256",
            "linux_intel_url",
            "linux_intel_sha256",
        )
    )
    if mac_platform and linux_platform:
        multi_platform = True
    else:
        multi_platform = False
    if mac_platform and not linux_platform:
        sources = f'''  on_macos do
    on_arm do
      url "{args.macos_arm64_url}"
      sha256 "{args.macos_arm64_sha256}"
    end
    on_intel do
      url "{args.macos_intel_url}"
      sha256 "{args.macos_intel_sha256}"
    end
  end'''
    elif multi_platform:
        sources = f'''  on_macos do
    on_arm do
      url "{args.macos_arm64_url}"
      sha256 "{args.macos_arm64_sha256}"
    end
    on_intel do
      url "{args.macos_intel_url}"
      sha256 "{args.macos_intel_sha256}"
    end
  end

  on_linux do
    on_arm do
      url "{args.linux_arm64_url}"
      sha256 "{args.linux_arm64_sha256}"
    end
    on_intel do
      url "{args.linux_intel_url}"
      sha256 "{args.linux_intel_sha256}"
    end
  end'''
    elif args.url and args.sha256:
        architecture = '  depends_on arch: :arm64\n' if args.arm64_only else ''
        sources = f'''  url "{args.url}"
  sha256 "{args.sha256}"
{architecture}'''
    else:
        raise SystemExit("provide either --url and --sha256 or platform URL and checksum pairs")
    formula = f'''class {args.formula_name} < Formula
  desc "Local first computer control runtime"
  homepage "https://github.com/Praket7/Comptrol"
{sources}
  version "{args.version}"
  license "Apache-2.0"
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

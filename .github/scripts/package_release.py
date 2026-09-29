"""Validate a release tag or main snapshot, package binaries, and checksum archives.

Uses only Python 3.11+ standard libraries on every GitHub-hosted build platform.
Artifacts live under target/dist, which is already ignored by Git.
"""

import argparse
import hashlib
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tomllib


ROOT = Path(__file__).resolve().parents[2]
DIST = ROOT / "target" / "dist"
# Keep these in sync with the native runner matrix in release.yml.
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
)


def build_label(version, tag=None, snapshot=None):
    if snapshot is not None:
        if not re.fullmatch(r"[0-9a-f]{40}", snapshot):
            raise ValueError("Snapshot must be a full 40-character commit SHA")
        return f"v{version}-main-{snapshot[:12]}"
    if tag != f"v{version}":
        raise ValueError(f"Tag must match Cargo.toml version: expected v{version}, got {tag}")
    return tag


def archive_name(tag, target):
    extension = "tar.gz"
    return f"diet_soda-{tag}-{target}.{extension}"


def package(tag, version, target):
    executable = "diet_soda"
    binary = ROOT / "target" / target / "release" / executable
    # Exercise the actual optimized binary without credentials, network, or a TTY.
    result = subprocess.run([binary, "--version"], check=True, capture_output=True, text=True)
    if result.stdout.strip() != f"diet_soda {version}":
        raise ValueError(f"Binary version does not match {tag}: {result.stdout.strip()}")
    subprocess.run([binary, "--help"], check=True, stdout=subprocess.DEVNULL)
    subprocess.run(
        [binary, "--config", ROOT / "examples" / "config.json", "--validate-config"],
        check=True,
    )

    # An explicit file list keeps local configs, sessions, and build files out.
    files = [(binary, Path(executable))]
    files += [(ROOT / name, Path(name)) for name in ("README.md", "LICENSE")]
    files += [
        (path, path.relative_to(ROOT))
        for path in sorted((ROOT / "examples").rglob("*"))
        if path.is_file() and "__pycache__" not in path.parts and path.name != ".DS_Store"
    ]
    DIST.mkdir(parents=True, exist_ok=True)
    archive = DIST / archive_name(tag, target)
    prefix = Path(f"diet_soda-{tag}-{target}")
    with tarfile.open(archive, "w:gz") as output:
        for source, relative in files:
            output.add(source, arcname=(prefix / relative).as_posix(), recursive=False)
    print(f"Packaged {archive}")


def checksums(tag):
    lines = []
    # Require every platform before publishing, rather than a partial glob match.
    for name in sorted(archive_name(tag, target) for target in TARGETS):
        with (DIST / name).open("rb") as archive:
            digest = hashlib.file_digest(archive, "sha256").hexdigest()
        lines.append(f"{digest}  {name}\n")
    (DIST / "SHA256SUMS").write_text("".join(lines), encoding="utf-8", newline="\n")
    print(f"Wrote {DIST / 'SHA256SUMS'}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--tag")
    source.add_argument("--snapshot", help="Full main-branch commit SHA; does not publish a release")
    action = parser.add_mutually_exclusive_group()
    action.add_argument("--target", choices=TARGETS)
    action.add_argument("--checksums", action="store_true")
    args = parser.parse_args()
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["package"]["version"]
    try:
        label = build_label(version, args.tag, args.snapshot)
    except ValueError as error:
        parser.error(str(error))
    if args.target:
        package(label, version, args.target)
    elif args.checksums:
        checksums(label)
    else:
        print(f"Validated build {label}")
        if output := os.environ.get("GITHUB_OUTPUT"):
            with open(output, "a", encoding="utf-8") as file:
                file.write(f"label={label}\n")


if __name__ == "__main__":
    main()

"""Builds MXM Player's release archive for the platform it runs on.

    python packaging/package.py [--out dist] [--skip-build]

One archive per platform, which the MXM portal installs (or anybody unpacks by hand):
- Windows: a zip of the player, its command line and the docs.
- macOS: a universal app (Apple silicon and Intel), zipped with `ditto` so its signature survives.
  It's signed with a Developer ID and notarised when the signing settings below are set, and
  signed ad hoc otherwise.
- Linux: a tarball with a desktop entry and a script that installs it for the user.

Taken from newDAWn's `packaging/package.py` (GPL-3.0-or-later, the same owner), less the Windows
installer: the portal installs from the zip.

Signing settings, all optional, read from the environment:
- Windows: Microsoft's Artifact Signing, `ARTIFACT_SIGNING_DLIB` (its signtool plug-in,
  `Azure.CodeSigning.Dlib.dll`) and `ARTIFACT_SIGNING_METADATA` (the account's `metadata.json`).
- macOS: `MACOS_SIGNING_IDENTITY` (a "Developer ID Application" identity in the keychain), and for
  notarising `APPLE_ID`, `APPLE_TEAM_ID` and `APPLE_APP_PASSWORD` (an app-specific password).
"""

import argparse
import os
import platform
import plistlib
import shutil
import subprocess
import sys
import tarfile
import tomllib
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
NAME = "MXM Player"
APP_ID = "mxm-player"
BUNDLE_ID = "dk.mxm.mxm-player"
# The player, and the command line that drives it over loopback (it ships: apps/mxm-player/AGENTS.md).
PROGRAMS = ["mxm-player", "mxm-cli"]
DOCS = ["LICENSE", "NOTICE.md", "README.md", "TRADEMARKS.md"]
MACOS_TARGETS = ["aarch64-apple-darwin", "x86_64-apple-darwin"]
MACOS_MINIMUM = "11.0"


def version() -> str:
    with open(ROOT / "apps" / "mxm-player" / "Cargo.toml", "rb") as manifest:
        return tomllib.load(manifest)["package"]["version"]


def fill(template: str, names: dict) -> str:
    """Puts the names into a template's @NAME@ markers."""
    for key, value in names.items():
        template = template.replace(f"@{key}@", value)
    return template


def run(command: list, **options) -> None:
    print("+", " ".join(str(part) for part in command), flush=True)
    subprocess.run([str(part) for part in command], check=True, **options)


def fresh(folder: Path) -> Path:
    shutil.rmtree(folder, ignore_errors=True)
    folder.mkdir(parents=True)
    return folder


def target_dir() -> Path:
    return Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))


def build(skip: bool, target: str | None = None) -> Path:
    """Builds the programs in release, and returns the folder they're in."""
    command = ["cargo", "build", "--locked", "--release", "-p", "mxm-player", "--bins"]
    if target:
        command += ["--target", target]
    if not skip:
        run(command, cwd=ROOT)
    return target_dir() / target / "release" if target else target_dir() / "release"


def copy_docs(folder: Path) -> None:
    for doc in DOCS:
        shutil.copy2(ROOT / doc, folder / doc)


def stem(system: str) -> str:
    return f"{APP_ID}-{version()}-{system}"


# Windows -----------------------------------------------------------------------------------------


def find_tool(name: str, candidates: list) -> Path | None:
    """A tool on the PATH, or else the first of `candidates` that exists."""
    found = shutil.which(name)
    if found:
        return Path(found)
    return next((candidate for candidate in candidates if candidate.exists()), None)


def sign_windows(files: list) -> None:
    dlib = os.environ.get("ARTIFACT_SIGNING_DLIB")
    metadata = os.environ.get("ARTIFACT_SIGNING_METADATA")
    if not (dlib and metadata):
        print("Not signing: ARTIFACT_SIGNING_DLIB and ARTIFACT_SIGNING_METADATA aren't both set.")
        return
    kits = Path(os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")) / "Windows Kits"
    # The newest Windows SDK first.
    signtool = find_tool("signtool", sorted(kits.glob("10/bin/*/x64/signtool.exe"), reverse=True))
    if not signtool:
        sys.exit("package: signtool isn't installed (it comes with the Windows SDK)")
    run([
        signtool, "sign", "/v", "/fd", "SHA256", "/tr", "http://timestamp.acs.microsoft.com",
        "/td", "SHA256", "/dlib", dlib, "/dmdf", metadata, *files,
    ])


def windows(out: Path, skip_build: bool) -> None:
    built = build(skip_build)
    name = stem("windows-x64")
    staging = fresh(out / name)
    executables = [shutil.copy2(built / f"{program}.exe", staging) for program in PROGRAMS]
    copy_docs(staging)
    sign_windows(executables)
    archive = out / f"{name}.zip"
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as zipped:
        for file in sorted(staging.rglob("*")):
            zipped.write(file, Path(name) / file.relative_to(staging))
    print(f"Made {archive}")


# macOS -------------------------------------------------------------------------------------------

# The hardened runtime, loosened only where a plugin host needs it: plugins are signed by other
# developers, and some generate code as they run. The player records nothing, so no audio input.
ENTITLEMENTS = {
    "com.apple.security.cs.disable-library-validation": True,
    "com.apple.security.cs.allow-unsigned-executable-memory": True,
}


def macos(out: Path, skip_build: bool) -> None:
    os.environ.setdefault("MACOSX_DEPLOYMENT_TARGET", MACOS_MINIMUM)
    folders = [build(skip_build, target) for target in MACOS_TARGETS]
    name = stem("macos-universal")
    staging = fresh(out / name)
    app = staging / f"{NAME}.app"
    executables = app / "Contents" / "MacOS"
    resources = app / "Contents" / "Resources"
    executables.mkdir(parents=True)
    resources.mkdir(parents=True)
    for program in PROGRAMS:
        run(["lipo", "-create", "-output", executables / program, *(f / program for f in folders)])
    copy_docs(resources)
    with open(app / "Contents" / "Info.plist", "wb") as plist:
        plistlib.dump({
            "CFBundleName": NAME,
            "CFBundleDisplayName": NAME,
            "CFBundleIdentifier": BUNDLE_ID,
            "CFBundleVersion": version(),
            "CFBundleShortVersionString": version(),
            "CFBundleExecutable": APP_ID,
            "CFBundlePackageType": "APPL",
            "CFBundleInfoDictionaryVersion": "6.0",
            "LSMinimumSystemVersion": MACOS_MINIMUM,
            "LSApplicationCategoryType": "public.app-category.music",
            "NSHighResolutionCapable": True,
        }, plist)

    identity = os.environ.get("MACOS_SIGNING_IDENTITY")
    if identity:
        entitlements = out / "entitlements.plist"
        with open(entitlements, "wb") as file:
            plistlib.dump(ENTITLEMENTS, file)
        sign = ["codesign", "--force", "--timestamp", "--options", "runtime",
                "--entitlements", entitlements, "--sign", identity]
        # The helper first, then the bundle, whose signature covers it.
        run([*sign, executables / "mxm-cli"])
        run([*sign, app])
    else:
        print("Signing ad hoc: MACOS_SIGNING_IDENTITY isn't set.")
        run(["codesign", "--force", "--deep", "--sign", "-", app])

    archive = out / f"{name}.zip"
    archive.unlink(missing_ok=True)
    zip_app = ["ditto", "-c", "-k", "--keepParent", app, archive]
    run(zip_app)
    account = [os.environ.get(key) for key in ("APPLE_ID", "APPLE_TEAM_ID", "APPLE_APP_PASSWORD")]
    if identity and all(account):
        run(["xcrun", "notarytool", "submit", archive, "--apple-id", account[0],
             "--team-id", account[1], "--password", account[2], "--wait"])
        run(["xcrun", "stapler", "staple", app])
        # Zipped again, so the archive carries the stapled ticket.
        archive.unlink()
        run(zip_app)
    elif identity:
        print("Not notarising: APPLE_ID, APPLE_TEAM_ID and APPLE_APP_PASSWORD aren't all set.")
    print(f"Made {archive}")


# Linux -------------------------------------------------------------------------------------------

DESKTOP = """[Desktop Entry]
Type=Application
Name=@NAME@
Comment=A host for the MXM instruments and effects
Exec=@APP_ID@
Terminal=false
Categories=AudioVideo;Audio;Midi;
StartupWMClass=@APP_ID@
"""

INSTALL = """#!/bin/sh
# Installs @NAME@ for this user: the programs into ~/.local/bin, and an entry in the app menu.
# Running it again updates them.
set -e
here=$(cd "$(dirname "$0")" && pwd)
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
menu="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$bin" "$menu"
for program in @PROGRAMS@; do
    install -m 755 "$here/$program" "$bin/$program"
done
sed "s|^Exec=.*|Exec=$bin/@APP_ID@|" "$here/@APP_ID@.desktop" > "$menu/@APP_ID@.desktop"
echo "Installed @NAME@ into $bin, with an entry in the app menu."
"""


def linux(out: Path, skip_build: bool) -> None:
    built = build(skip_build)
    machine = {"x86_64": "x64", "aarch64": "arm64"}.get(platform.machine(), platform.machine())
    name = stem(f"linux-{machine}")
    staging = fresh(out / name)
    for program in PROGRAMS:
        shutil.copy2(built / program, staging / program)
    copy_docs(staging)
    names = {"NAME": NAME, "APP_ID": APP_ID, "PROGRAMS": " ".join(PROGRAMS)}
    (staging / f"{APP_ID}.desktop").write_text(fill(DESKTOP, names), encoding="utf-8")
    install = staging / "install.sh"
    install.write_text(fill(INSTALL, names), encoding="utf-8")
    install.chmod(0o755)
    archive = out / f"{name}.tar.gz"
    with tarfile.open(archive, "w:gz") as tarball:
        tarball.add(staging, arcname=name)
    print(f"Made {archive}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=Path, default=ROOT / "dist", help="where the archive goes")
    parser.add_argument("--skip-build", action="store_true", help="package what's already built")
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    system = platform.system()
    if system == "Windows":
        windows(out, args.skip_build)
    elif system == "Darwin":
        macos(out, args.skip_build)
    elif system == "Linux":
        linux(out, args.skip_build)
    else:
        sys.exit(f"package: there is no archive for {system}")


if __name__ == "__main__":
    main()

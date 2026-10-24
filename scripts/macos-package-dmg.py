#!/usr/bin/env python3
"""Create a verified DMG from a released ARM64 ZIP and the repository installer."""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import tempfile
import zipfile


INSTALL = b'''#!/bin/bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.payload"
exec /bin/bash scripts/macos-install.sh --no-build "$@"
'''

INSTRUCTIONS = """Pool Router {version} | macOS 11+ | Apple Silicon

安装
1. 双击本磁盘映像中的“安装.command”。
2. 等待终端显示安装完成，浏览器会打开本地控制台。
3. 安装完成后可推出磁盘映像。以后打开 ~/Applications/Pool Router.app。

镜像包含完整 ARM64 程序、安装脚本、文档及依赖许可证，无须编译。
安装器使用 macOS 自带工具，无须另外安装 Python。它安装当前用户的后台服务和启动器；
重新安装会重启本软件的后台服务。
安装本身不会修改 Codex 或 CC Switch 接入配置。
首次使用请在控制台配置来源、验证账号，再执行 Codex 直连接入。
本镜像未进行 Apple 公证。首次打开若被系统阻止，请在系统设置的
“隐私与安全性”中找到本次拦截提示，核对来源后选择“仍要打开”。
macOS 11/12 对应入口为“系统偏好设置 > 安全性与隐私 > 通用”。

Installation
1. Double-click 安装.command in this disk image.
2. Wait for installation to complete; the local dashboard opens in your browser.
3. Eject the image afterward. Launch ~/Applications/Pool Router.app next time.

This image contains the complete ARM64 executable, installer, documentation,
and dependency licenses. The installer uses built-in macOS tools; no compilation
or Python installation is required. Reinstalling restarts the background service.
Installation does not change Codex or CC Switch connection settings.
This image is not notarized by Apple. If macOS blocks the first launch, review
the source and use Open Anyway in System Settings > Privacy & Security.
On macOS 11/12, use System Preferences > Security & Privacy > General.
"""


def run(*arguments):
    return subprocess.check_output(arguments, stderr=subprocess.STDOUT)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def build(archive_path, destination, work_dir):
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        raise ValueError("Output already exists; choose a new DMG path.")
    if work_dir is not None:
        work_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="pool-router-dmg-", dir=work_dir) as temporary:
        temporary = Path(temporary)
        root = temporary / "image"
        payload = root / ".payload"
        payload.mkdir(parents=True)
        expected = {}
        with zipfile.ZipFile(archive_path) as archive:
            if archive.testzip() is not None:
                raise ValueError("Input ZIP checksum verification failed.")
            manifest = json.loads(archive.read("PoolRouter/MANIFEST.json"))
            if manifest.get("kind") != "binary" or manifest.get("target") != "aarch64-apple-darwin":
                raise ValueError("The input must be the macOS ARM64 binary ZIP.")
            version = manifest["version"]
            if not re.fullmatch(r"[0-9][0-9A-Za-z.+-]*", version):
                raise ValueError("Invalid release version.")
            entries = archive.infolist()
            if len({entry.filename for entry in entries}) != len(entries):
                raise ValueError("Duplicate ZIP entries are not supported.")
            for entry in entries:
                path = PurePosixPath(entry.filename)
                if path.is_absolute() or ".." in path.parts or path.parts[:1] != ("PoolRouter",):
                    raise ValueError("Invalid path in input ZIP.")
                if stat.S_ISLNK(entry.external_attr >> 16):
                    raise ValueError("Symlinks are not supported in the release ZIP.")
                if entry.is_dir():
                    continue
                relative = Path(*path.parts[1:])
                if relative.as_posix() in expected:
                    raise ValueError("Duplicate normalized ZIP paths are not supported.")
                data = archive.read(entry)
                target = payload / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(data)
                target.chmod(0o755 if entry.external_attr >> 16 & 0o111 else 0o644)
                expected[relative.as_posix()] = digest(data)
            declared = {item["path"] for item in manifest["files"]}
            if len(declared) != len(manifest["files"]) or declared != expected.keys() - {"MANIFEST.json"}:
                raise ValueError("Input manifest must cover every payload file exactly once.")
            for item in manifest["files"]:
                if expected.get(item["path"]) != item["sha256"]:
                    raise ValueError("Input ZIP does not match its release manifest.")
        required = {"target/release/pool-router", "scripts/macos-install.sh",
                    "scripts/macos-uninstall.sh", "THIRD_PARTY_NOTICES.md"}
        if not required <= expected.keys():
            raise ValueError("The release ZIP is missing installation files.")
        header = (payload / "target/release/pool-router").read_bytes()[:8]
        if header != b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01":
            raise ValueError("The packaged executable is not ARM64 Mach-O.")
        binary_checksum = expected["target/release/pool-router"]
        # Preserve the published program and licenses. Only the installer and
        # its documentation are updated, with their provenance in the manifest.
        repository = Path(__file__).resolve().parent.parent
        updated_files = []
        for name in ("scripts/macos-install.sh", "README.md", "README.en.md"):
            data = (repository / name).read_bytes()
            checksum = digest(data)
            if checksum != expected[name]:
                updated_files.append({"path": name, "previous_sha256": expected[name], "sha256": checksum})
            (payload / name).write_bytes(data)
            expected[name] = checksum
        manifest["packaging"] = {
            "format": "dmg",
            "source_zip_sha256": digest(archive_path.read_bytes()),
            "updated_files": updated_files,
        }
        for item in manifest["files"]:
            item["sha256"] = expected[item["path"]]
            item["size"] = (payload / item["path"]).stat().st_size
        manifest_data = (json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode()
        (payload / "MANIFEST.json").write_bytes(manifest_data)
        expected["MANIFEST.json"] = digest(manifest_data)
        (root / "安装.command").write_bytes(INSTALL)
        (root / "安装.command").chmod(0o755)
        guide = INSTRUCTIONS.format(version=version).encode()
        (root / "安装说明.txt").write_bytes(guide)
        image = temporary / "PoolRouter-macOS-arm64.dmg"
        run("hdiutil", "create", "-srcfolder", str(root), "-volname", f"Pool Router {version}",
            "-fs", "HFS+", "-format", "UDZO", "-imagekey", "zlib-level=9", "-nospotlight", str(image))
        run("hdiutil", "verify", str(image))
        mount = temporary / "mounted image"
        mount.mkdir()
        attached = False
        try:
            run("hdiutil", "attach", "-readonly", "-nobrowse", "-noautoopen", "-mountpoint", str(mount), str(image))
            attached = True
            if not os.statvfs(mount).f_flag & os.ST_RDONLY:
                raise ValueError("The test volume is not mounted read-only.")
            if (mount / "安装.command").read_bytes() != INSTALL or (mount / "安装说明.txt").read_bytes() != guide:
                raise ValueError("DMG entry points do not match the prepared files.")
            for name, checksum in expected.items():
                if digest((mount / ".payload" / name).read_bytes()) != checksum:
                    raise ValueError("DMG payload differs from the prepared release manifest.")
            for name in ("安装.command", ".payload/scripts/macos-install.sh", ".payload/scripts/macos-uninstall.sh"):
                if not os.access(mount / name, os.X_OK):
                    raise ValueError("An installer script lost executable permissions.")
                run("/bin/bash", "-n", str(mount / name))
            executable = mount / ".payload/target/release/pool-router"
            if digest(executable.read_bytes()) != binary_checksum:
                raise ValueError("DMG executable differs from the published ZIP.")
            run("/usr/bin/codesign", "--verify", "--strict", str(executable))
            if run(str(executable), "--version").decode().strip() != f"Pool Router {version}":
                raise ValueError("Packaged executable version mismatch.")
        finally:
            if attached:
                run("hdiutil", "detach", str(mount))
        with tempfile.NamedTemporaryFile(prefix=".pool-router-", suffix=".dmg", dir=destination.parent, delete=False) as staging:
            staging_path = Path(staging.name)
        try:
            shutil.copyfile(image, staging_path)
            staging_path.chmod(0o644)
            os.replace(staging_path, destination)
        finally:
            staging_path.unlink(missing_ok=True)
        print(f"Verified read-only DMG: {destination.name}; {len(expected)} verified payload files.")
        print(f"Published executable unchanged; updated installer/documentation files: {len(updated_files)}.")
        print(f"{digest(destination.read_bytes())}  {destination.name}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary_zip", type=Path)
    parser.add_argument("output_dmg", type=Path)
    parser.add_argument("--work-dir", type=Path)
    arguments = parser.parse_args()
    build(arguments.binary_zip.resolve(), arguments.output_dmg.resolve(), arguments.work_dir)

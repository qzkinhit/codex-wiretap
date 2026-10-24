#!/usr/bin/env python3
"""打包已完成构建的发布文件，不构建、安装、启动服务或上传。"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import os
import re
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import time
import zipfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

MINIMUM_PYTHON = (3, 9)
DEFAULT_REPO = Path(__file__).resolve().parent.parent
TARGET = "aarch64-apple-darwin"
MANIFEST = "MANIFEST.json"
SKIP_DIRS = {".git", "target", "data", ".local", "dist", "work", "outputs", "node_modules", "__pycache__", "__MACOSX", ".codex", "logs"}
PRIVATE_PATTERNS = (
    "config.json", "settings.json", "manual-keys.json", "records.json*", "keys.json",
    "auth.json", "credentials*", "secrets*", "*.key", "*.pem", "*.p12", "*.pfx",
    "integration-backup*.json", "integration-restored-*.json", "cc-monitor-backup*.json",
    "cc-monitor-restored-*.json", "*.log", "*.jsonl", "*.sqlite*", "*.db", "*.pyc", "*.pyo",
    "*.zip", "*.tar", "*.tar.gz", "*.dmg", "*.bak", "*.tmp",
)
LICENSE_NAME = re.compile(r"^(?:licen[cs]e|copying|notice|copyright|unlicense)(?:[._-].*)?$", re.I)
INSTALL_COMMAND = b'''#!/bin/bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
exec bash scripts/macos-install.sh --no-build "$@"
'''


class PackagingError(RuntimeError):
    pass


@dataclass(frozen=True)
class Entry:
    name: str
    source: Path | None = None
    data: bytes | None = None
    mode: int = 0o644

    def read(self) -> bytes:
        return self.source.read_bytes() if self.source is not None else self.data or b""


def run(repo: Path, arguments: list[str]) -> bytes:
    result = subprocess.run(arguments, cwd=repo, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode:
        # 工具输出可能包含本机路径，不写入任何发布清单。
        raise PackagingError(f"命令失败（退出码 {result.returncode}）：{arguments[0]} {arguments[1]}。请在仓库中检查该命令。")
    return result.stdout


def safe_name(name: str) -> str:
    parts = PurePosixPath(name).parts
    if not parts or name.startswith("/") or any(part in {"", ".", ".."} for part in parts) or "\\" in name or "\x00" in name:
        raise PackagingError("压缩包包含不安全的相对路径。")
    return name


def excluded(name: str) -> bool:
    path = PurePosixPath(name)
    parts = path.parts
    if any(part in SKIP_DIRS or part == ".DS_Store" or part.startswith("._") for part in parts):
        return True
    basename = path.name.lower()
    if basename.startswith(".env") and basename != ".env.example":
        return True
    return any(fnmatch.fnmatch(basename, pattern) for pattern in PRIVATE_PATTERNS)


def file_entry(root: Path, name: str, *, archive_name: str | None = None, executable: bool = False) -> Entry:
    safe_name(name)
    source = root / name
    if not source.is_file() or source.is_symlink() or not source.resolve().is_relative_to(root.resolve()):
        raise PackagingError(f"文件缺失、为符号链接或超出允许目录：{name}")
    mode = 0o755 if executable or source.stat().st_mode & 0o111 else 0o644
    return Entry(safe_name(archive_name or name), source=source, mode=mode)


def repository_entries(repo: Path) -> list[Entry]:
    result = run(repo, ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"])
    names = sorted({raw.decode("utf-8") for raw in result.split(b"\x00") if raw})
    entries = []
    for name in names:
        safe_name(name)
        if excluded(name) or not (repo / name).exists():
            continue
        entries.append(file_entry(repo, name))
    included = {entry.name for entry in entries}
    required = {"Cargo.toml", "Cargo.lock", "src/lib.rs", "src/main.rs", "web/app.js", "web/index.html", "web/style.css", "scripts/macos-install.sh", "scripts/macos-uninstall.sh", "README.md", "README.en.md", "SECURITY.md", "LICENSE", "打开控制台.command"}
    missing = required - included
    if missing:
        raise PackagingError("源码包缺少必要文件：" + ", ".join(sorted(missing)))
    # Git 会返回未暂存的新文件；额外核对根模块，避免漏掉新增 Rust 模块。
    library = (repo / "src/lib.rs").read_text(encoding="utf-8")
    for module in re.findall(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;", library, re.M):
        alternatives = {f"src/{module}.rs", f"src/{module}/mod.rs"}
        if not alternatives & included:
            raise PackagingError(f"源码包缺少模块：{module}")
    if not any(name.startswith(".github/") for name in included):
        raise PackagingError("源码包缺少 .github 工作流。")
    return entries


def dependency_entries(repo: Path) -> tuple[list[Entry], str]:
    # 仅解析锁定的依赖元数据，不执行构建；缺少缓存时由 Cargo 获取依赖包。
    metadata = json.loads(run(repo, ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", TARGET]))
    packages = metadata.get("packages", [])
    root_manifest = (repo / "Cargo.toml").resolve()
    root_package = next((package for package in packages if Path(package["manifest_path"]).resolve() == root_manifest), None)
    if root_package is None:
        raise PackagingError("Cargo metadata 未返回当前发布包。")
    dependencies = [package for package in packages if package["id"] != root_package["id"]]
    entries: list[Entry] = []
    manifest_packages = []
    seen_destinations: set[str] = set()
    missing_licenses = []
    for package in sorted(dependencies, key=lambda item: (item["name"], item["version"], item["id"])):
        crate_root = Path(package["manifest_path"]).resolve().parent
        slug = f"{package['name']}-{package['version']}"
        if slug in seen_destinations:
            slug += "-" + hashlib.sha256(package["id"].encode()).hexdigest()[:10]
        seen_destinations.add(slug)
        discovered: set[Path] = set()
        for current, directories, files in os.walk(crate_root, followlinks=False):
            directories[:] = sorted(directory for directory in directories if directory not in SKIP_DIRS and not (Path(current) / directory).is_symlink())
            for filename in files:
                if LICENSE_NAME.fullmatch(filename):
                    discovered.add(Path(current) / filename)
        if package.get("license_file"):
            stated = Path(package["license_file"])
            discovered.add(stated if stated.is_absolute() else crate_root / stated)
        license_files = []
        for license_path in sorted(discovered):
            resolved = license_path.resolve()
            if not resolved.is_relative_to(crate_root) or not resolved.is_file() or license_path.is_symlink():
                raise PackagingError(f"依赖 {slug} 的许可证路径异常。")
            relative = resolved.relative_to(crate_root).as_posix()
            destination = safe_name(f"third-party/licenses/{slug}/{relative}")
            entries.append(Entry(destination, source=resolved))
            license_files.append(destination)
        if not license_files:
            missing_licenses.append(slug)
        source = package.get("source") or ""
        manifest_packages.append({
            "name": package["name"], "version": package["version"],
            "license": package.get("license"),
            "source_kind": "registry" if source.startswith("registry+") else "git" if source.startswith("git+") else "path",
            "license_files": license_files,
        })
    if missing_licenses:
        raise PackagingError("以下依赖未找到 LICENSE/COPYING/NOTICE 等许可文件，停止打包：" + ", ".join(missing_licenses))
    dependency_manifest = {
        "schema_version": 1, "target": TARGET,
        "scope": "Cargo locked metadata; includes build, test and platform support dependencies where present.",
        "packages": manifest_packages,
    }
    entries.append(Entry("third-party/DEPENDENCIES.json", data=json_bytes(dependency_manifest)))
    notices = [
        "# Third-party notices", "",
        "This distribution includes third-party Rust dependencies and bundled native components.",
        "The inventory is derived from locked Cargo metadata for aarch64-apple-darwin. It may also include build, test, and platform support dependencies; inclusion does not assert that every package is linked into the executable.",
        "License identifiers below come from package metadata. Original LICENSE, COPYING, NOTICE, COPYRIGHT, or equivalent files are included without modification under third-party/licenses/.",
        "The machine-readable inventory is third-party/DEPENDENCIES.json. All referenced paths are relative to this distribution.", "",
    ]
    for package in manifest_packages:
        notices.extend([f"## {package['name']} {package['version']}", "", f"Declared license: {package['license'] or 'See included license files'}", ""])
        notices.extend(f"- {name}" for name in package["license_files"])
        notices.append("")
    entries.append(Entry("THIRD_PARTY_NOTICES.md", data=("\n".join(notices) + "\n").encode("utf-8")))
    return entries, root_package["version"]


def verify_binary(binary: Path) -> None:
    if not binary.is_file() or binary.is_symlink() or not os.access(binary, os.X_OK):
        raise PackagingError("缺少可执行的 target/release/pool-router；请先完成 release 构建。")
    with binary.open("rb") as stream:
        header = stream.read(12)
    # 仅接受 macOS ARM64 的薄 Mach-O，避免把其他平台文件误标成 arm64 包。
    if len(header) < 12 or header[:4] != b"\xcf\xfa\xed\xfe" or struct.unpack("<I", header[4:8])[0] != 0x0100000C:
        raise PackagingError("release 二进制不是 macOS ARM64 Mach-O 文件。")


def json_bytes(value: object) -> bytes:
    return (json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode("utf-8")


def hash_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def file_hash(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_archive(destination: Path, top: str, entries: list[Entry], version: str, kind: str, timestamp: tuple[int, ...]) -> None:
    names = [entry.name for entry in entries]
    if len(names) != len(set(names)) or MANIFEST in names:
        raise PackagingError("发布文件名冲突。")
    manifest_files = []
    with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9, allowZip64=True) as archive:
        for entry in sorted(entries, key=lambda item: item.name):
            name = safe_name(f"{top}/{entry.name}")
            data = entry.read()
            info = zipfile.ZipInfo(name, date_time=timestamp)
            info.create_system = 3
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (stat.S_IFREG | entry.mode) << 16
            archive.writestr(info, data, compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)
            manifest_files.append({"path": entry.name, "size": len(data), "sha256": hash_bytes(data), "mode": f"{entry.mode:04o}"})
        manifest = {
            "schema_version": 1, "product": "Pool Router", "version": version,
            "kind": kind, "target": TARGET if kind == "binary" else None,
            "manifest_excludes_itself": True, "files": manifest_files,
        }
        info = zipfile.ZipInfo(f"{top}/{MANIFEST}", date_time=timestamp)
        info.create_system = 3
        info.external_attr = (stat.S_IFREG | 0o644) << 16
        archive.writestr(info, json_bytes(manifest), compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)
    validate_archive(destination, top, {entry.name for entry in entries}, kind)


def validate_archive(path: Path, top: str, expected: set[str], kind: str) -> None:
    with zipfile.ZipFile(path) as archive:
        if archive.testzip() is not None:
            raise PackagingError("ZIP CRC 校验失败。")
        names = archive.namelist()
        desired = {f"{top}/{name}" for name in expected | {MANIFEST}}
        if set(names) != desired or len(names) != len(desired):
            raise PackagingError("ZIP 条目与目标清单不一致。")
        manifest = json.loads(archive.read(f"{top}/{MANIFEST}"))
        if {item["path"] for item in manifest["files"]} != expected:
            raise PackagingError("MANIFEST 文件列表与 ZIP 不一致。")
        for item in manifest["files"]:
            name = safe_name(item["path"])
            if Path(name).is_absolute() or name.startswith(("Users/", "home/")):
                raise PackagingError("MANIFEST 不得包含本机路径。")
            data = archive.read(f"{top}/{name}")
            info = archive.getinfo(f"{top}/{name}")
            if len(data) != item["size"] or hash_bytes(data) != item["sha256"]:
                raise PackagingError(f"MANIFEST 内容校验失败：{name}")
            if (info.external_attr >> 16) & 0o777 != int(item["mode"], 8):
                raise PackagingError(f"ZIP 权限校验失败：{name}")
            if any(ord(character) > 127 for character in name) and not info.flag_bits & 0x800:
                raise PackagingError(f"ZIP UTF-8 标识缺失：{name}")
            if any(part == ".DS_Store" or part.startswith("._") or part == "__MACOSX" for part in PurePosixPath(name).parts):
                raise PackagingError("ZIP 含有系统附加文件。")
        dependencies = json.loads(archive.read(f"{top}/third-party/DEPENDENCIES.json"))
        for package in dependencies["packages"]:
            for license_file in package["license_files"]:
                if license_file not in expected:
                    raise PackagingError("依赖清单引用了不存在的许可证。")
        required_executables = {"scripts/macos-install.sh", "scripts/macos-uninstall.sh", "打开控制台.command"}
        if kind == "binary":
            required_executables |= {"target/release/pool-router", "安装.command"}
        for name in required_executables:
            if not archive.getinfo(f"{top}/{name}").external_attr >> 16 & 0o111:
                raise PackagingError(f"可执行权限缺失：{name}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=DEFAULT_REPO, help="已构建的源码仓库，默认使用此脚本所在仓库")
    parser.add_argument("--output", type=Path, help="发布文件输出目录，默认为所选仓库的 dist/macos")
    parser.add_argument("--work-dir", type=Path, help="临时打包目录，默认为所选仓库的 dist/package-work")
    args = parser.parse_args()
    repo = args.repo.resolve()
    output = (args.output or repo / "dist/macos").resolve()
    work_root = (args.work_dir or repo / "dist/package-work").resolve()
    source = repository_entries(repo)
    binary = repo / "target/release/pool-router"
    verify_binary(binary)
    licenses, version = dependency_entries(repo)
    source_names = {entry.name for entry in source}
    screenshots = sorted(name for name in source_names if name.startswith("docs/") and Path(name).suffix.lower() in {".png", ".jpg", ".jpeg", ".webp"})
    if not {"docs/dashboard.png", "docs/monitor.png"} <= set(screenshots):
        raise PackagingError("缺少更新后的 dashboard.png 或 monitor.png 截图。")
    binary_names = ["README.md", "README.en.md", "SECURITY.md", "LICENSE", "scripts/macos-install.sh", "scripts/macos-uninstall.sh", "打开控制台.command", *screenshots]
    binary_entries = [file_entry(repo, name, executable=name.endswith((".sh", ".command"))) for name in binary_names]
    binary_entries += [file_entry(repo, "target/release/pool-router", executable=True), Entry("安装.command", data=INSTALL_COMMAND, mode=0o755), *licenses]
    source = [Entry(entry.name, entry.source, entry.data, 0o755 if entry.name.endswith((".sh", ".command")) else entry.mode) for entry in source] + licenses
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH") or run(repo, ["git", "log", "-1", "--format=%ct"]).decode().strip())
    timestamp = time.gmtime(max(315532800, min(epoch, 4354819198)))[:6]
    work_root.mkdir(parents=True, exist_ok=True)
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="package-", dir=work_root) as temporary:
        temporary_root = Path(temporary)
        binary_zip = temporary_root / "PoolRouter-macOS-arm64.zip"
        source_zip = temporary_root / "PoolRouter-source.zip"
        write_archive(binary_zip, "PoolRouter", binary_entries, version, "binary", timestamp)
        write_archive(source_zip, "pool-router", source, version, "source", timestamp)
        sums = "".join(f"{file_hash(artifact)}  {artifact.name}\n" for artifact in [binary_zip, source_zip])
        checksum_file = temporary_root / "SHA256SUMS"
        checksum_file.write_text(sums, encoding="utf-8")
        # 所有 ZIP 和引用均校验完成后才替换交付文件。
        for artifact in [binary_zip, source_zip, checksum_file]:
            staged = output / f".{artifact.name}.tmp"
            shutil.copyfile(artifact, staged)
            os.replace(staged, output / artifact.name)
    print(f"打包完成。版本 {version}，源码条目 {len(source)}，二进制包条目 {len(binary_entries)}。")
    print("已验证 ZIP CRC、SHA-256、MANIFEST、依赖许可证引用、可执行权限与 UTF-8 文件名。")
    print("输出文件：PoolRouter-macOS-arm64.zip、PoolRouter-source.zip、SHA256SUMS。尚未上传。")
    return 0


if __name__ == "__main__":
    if sys.version_info < MINIMUM_PYTHON:
        print(
            "需要 Python 3.9 或更新版本；macOS 自带的旧 python3 没有 Path.is_relative_to。",
            file=sys.stderr,
        )
        raise SystemExit(1)
    try:
        raise SystemExit(main())
    except (PackagingError, OSError, ValueError, KeyError, zipfile.BadZipFile) as error:
        print(f"打包失败：{error}", file=sys.stderr)
        raise SystemExit(1)

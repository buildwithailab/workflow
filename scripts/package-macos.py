#!/usr/bin/env python3
"""Validate and package the local ad-hoc Apple Silicon application."""
import hashlib
import json
import os
from contextlib import ExitStack
from pathlib import Path
import plistlib
import re
import shutil
import stat
import subprocess
import tempfile
import zipfile


MAX_FILE = 128 * 1024 * 1024
MAX_TOTAL = 1024 * 1024 * 1024
APP = "target/release/bundle/macos/Workflow.app"
ICON = "Contents/Resources/Workflow-Mac.icns"
PLIST = "Contents/Info.plist"
SIGNATURE = "Contents/_CodeSignature/CodeResources"
EXECUTABLES = {"Contents/MacOS/workflow", "Contents/MacOS/Workflow"}
DIRECTORIES = {"Contents", "Contents/MacOS", "Contents/Resources", "Contents/_CodeSignature"}
DOCUMENTS = {
    "LICENSE.txt": "LICENSE",
    "THIRD-PARTY-NOTICES.txt": "target/macos-third-party-notices.txt",
    "LICENSE-xterm.txt": "ui/vendor/LICENSE-xterm.txt",
    "LICENSE-addon-fit.txt": "ui/vendor/LICENSE-addon-fit.txt",
}
ZIP_NAME = "Workflow-Update-macOS-arm64.app.zip"
SUM_NAME = ZIP_NAME + ".sha256"
FEED_NAME = "Workflow-Update-macOS-arm64.json"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def check_type(path, directory=False):
    info = path.lstat()
    reparse = getattr(info, "st_file_attributes", 0) & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0)
    expected = stat.S_ISDIR if directory else stat.S_ISREG
    require(not reparse and expected(info.st_mode), "Expected an ordinary directory or regular file")
    return info


def input_path(root, relative, directory=False):
    path = root
    for part in Path(relative).parts[:-1]:
        path = path / part
        check_type(path, directory=True)
    path = root / relative
    info = check_type(path, directory)
    if not directory:
        require(info.st_size <= MAX_FILE, "Input exceeds the file size limit")
    return path


def read_input(root, relative):
    path = input_path(root, relative)
    with ExitStack() as handles:
        # macOS can anchor the walk in open directory descriptors. A link swapped
        # into the source path after preflight cannot redirect a later read.
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
        if os.open in os.supports_dir_fd:
            directory_flags = flags | os.O_DIRECTORY
            directory = os.open(root, directory_flags)
            handles.callback(os.close, directory)
            for part in Path(relative).parts[:-1]:
                directory = os.open(part, directory_flags, dir_fd=directory)
                handles.callback(os.close, directory)
            descriptor = os.open(path.name, flags, dir_fd=directory)
        else:
            descriptor = os.open(path, flags | getattr(os, "O_BINARY", 0))
        stream = handles.enter_context(os.fdopen(descriptor, "rb"))
        info = os.fstat(descriptor)
        require(stat.S_ISREG(info.st_mode) and info.st_size <= MAX_FILE, "Invalid input file")
        data = stream.read(info.st_size + 1)
        require(len(data) == info.st_size, "Input changed while being read")
        return data, stat.S_IMODE(info.st_mode) & 0o777


def scan_bundle(bundle, executable=None, complete=False):
    check_type(bundle, directory=True)
    programs = EXECUTABLES if executable is None else {"Contents/MacOS/" + executable}
    resources = {"Contents/Resources/" + name for name in DOCUMENTS}
    required = {PLIST, ICON, SIGNATURE}
    allowed = required | programs | resources
    files = set()
    directories = [bundle]
    total = 0
    while directories:
        directory = directories.pop()
        for entry in directory.iterdir():
            name = entry.relative_to(bundle).as_posix()
            if name in DIRECTORIES:
                check_type(entry, directory=True)
                directories.append(entry)
                continue
            require(name in allowed, "Unexpected application resource")
            info = check_type(entry)
            require(info.st_size <= MAX_FILE, "Application resource exceeds its size limit")
            total += info.st_size
            require(total <= MAX_TOTAL, "Application exceeds its expanded size limit")
            files.add(name)
    require(required <= files and len(programs & files) == 1, "Incomplete application bundle")
    if complete:
        require(resources <= files, "Missing packaged license documents")
    return sorted(files)


def verify_signature(bundle):
    subprocess.run(["/usr/bin/codesign", "--verify", "--deep", "--strict", "--all-architectures",
                    str(bundle)], check=True, capture_output=True)


def bundle_metadata(bundle, version, public, icon):
    metadata = plistlib.loads((bundle / PLIST).read_bytes())
    require(metadata["CFBundleIdentifier"] == "local.workflow.desktop", "Incorrect application identifier")
    require(metadata["CFBundleShortVersionString"] == metadata["CFBundleVersion"] == version,
            "Incorrect application version")
    executable = metadata["CFBundleExecutable"]
    require(executable in ("workflow", "Workflow"), "Unexpected application executable")
    require(metadata.get("CFBundleIconFile") == "Workflow-Mac.icns" and (bundle / ICON).read_bytes() == icon,
            "Rebuild with the current Mac icon")
    scan_bundle(bundle, executable)
    binary = (bundle / "Contents/MacOS" / executable).read_bytes()
    require(len(binary) >= 32 and binary[:4] == bytes.fromhex("cffaedfe")
            and int.from_bytes(binary[4:8], "little") == 0x0100000C
            and int.from_bytes(binary[12:16], "little") == 2, "Expected an Apple Silicon executable")
    require(public and public in binary, "Missing update verification key")
    require(b"test-control.json" not in binary, "Desktop test bridge must be absent")
    for content in (binary, binary[::2], binary[1::2]):
        require(not re.search(rb"/(?:Users|home)/[^/\s\x00]+/", content), "Rebuild with path remapping")
    return metadata


def check_outputs(dist):
    try:
        check_type(dist, directory=True)
    except FileNotFoundError:
        return
    for name in (ZIP_NAME, SUM_NAME, FEED_NAME):
        try:
            check_type(dist / name)
        except FileNotFoundError:
            pass


def install_outputs(dist, work, archive_path):
    # Withdraw the old authorization and checksum before replacing the ZIP.
    # A hard link keeps the old ZIP available for rollback across both renames.
    previous_zip = work / "previous.zip"
    moved = []
    replaced_zip = False
    if (dist / ZIP_NAME).exists():
        try:
            os.link(dist / ZIP_NAME, previous_zip)
        except OSError:
            shutil.copyfile(dist / ZIP_NAME, previous_zip)
    try:
        for name in (FEED_NAME, SUM_NAME):
            if (dist / name).exists():
                os.replace(dist / name, work / ("previous-" + name))
                moved.append(name)
        os.replace(archive_path, dist / ZIP_NAME)
        replaced_zip = True
        os.replace(work / SUM_NAME, dist / SUM_NAME)
    except OSError:
        if replaced_zip:
            if previous_zip.exists():
                os.replace(previous_zip, dist / ZIP_NAME)
            else:
                (dist / ZIP_NAME).unlink()
        for name in reversed(moved):
            os.replace(work / ("previous-" + name), dist / name)
        raise


def package(root):
    check_type(root, directory=True)
    app = input_path(root, APP, directory=True)
    # Check all source and support paths, including ancestors, before writing anything.
    source_files = scan_bundle(app)
    config, _ = read_input(root, "tauri.conf.json")
    public, _ = read_input(root, "src/update-public-key.txt")
    icon, _ = read_input(root, "target/macos-icons/Workflow-Mac.icns")
    documents = {name: read_input(root, relative)[0] for name, relative in DOCUMENTS.items()}
    dist = root / "dist"
    check_outputs(dist)
    version = json.loads(config)["version"]
    with tempfile.TemporaryDirectory(prefix="workflow-package-") as temporary:
        staged = Path(temporary) / "Workflow.app"
        for name in sorted(DIRECTORIES):
            (staged / name).mkdir(parents=True, exist_ok=True)
        for name in source_files:
            data, mode = read_input(root, APP + "/" + name)
            destination = staged / name
            destination.write_bytes(data)
            destination.chmod(mode)
        scan_bundle(staged)
        metadata = bundle_metadata(staged, version, public.strip(), icon)
        verify_signature(app)
        verify_signature(staged)
        signature = subprocess.run(["/usr/bin/codesign", "--display", "--verbose=4", str(staged)],
                                   check=True, capture_output=True).stderr
        require(b"Signature=adhoc" in signature.splitlines(), "Only local ad-hoc candidates can be packaged")
        executable = metadata["CFBundleExecutable"]
        scan_bundle(staged, executable)
        for name, content in documents.items():
            (staged / "Contents/Resources" / name).write_bytes(content)
        # The cross-platform updater reads XML. Re-sign the modified private copy.
        (staged / PLIST).write_bytes(plistlib.dumps(metadata, fmt=plistlib.FMT_XML, sort_keys=False))
        scan_bundle(staged, executable, complete=True)
        subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", "--deep", str(staged)],
                       check=True, capture_output=True)
        files = scan_bundle(staged, executable, complete=True)
        verify_signature(staged)

        dist.mkdir(exist_ok=True)
        check_outputs(dist)
        # Store temporary outputs on the destination volume for atomic replacement.
        with ExitStack() as cleanup:
            output = cleanup.enter_context(tempfile.TemporaryDirectory(prefix=".workflow-package-", dir=dist))
            work = Path(output)
            descriptor, name = tempfile.mkstemp(prefix=".workflow-update-", suffix=".zip", dir=dist)
            os.close(descriptor)
            archive_path = Path(name)
            cleanup.callback(archive_path.unlink, missing_ok=True)
            with zipfile.ZipFile(archive_path, "w", zipfile.ZIP_DEFLATED) as archive:
                for name in sorted(DIRECTORIES):
                    archive.write(staged / name, "Workflow.app/" + name + "/")
                for name in files:
                    archive.write(staged / name, "Workflow.app/" + name)
            require(64 <= archive_path.stat().st_size <= MAX_FILE, "ZIP exceeds the package size limit")
            with zipfile.ZipFile(archive_path) as archive:
                require(archive.testzip() is None, "ZIP integrity verification failed")
            digest = hashlib.sha256(archive_path.read_bytes()).hexdigest()
            (work / SUM_NAME).write_text(digest + "  " + ZIP_NAME + "\n", encoding="ascii")
            if os.name == "posix":
                import fcntl
                lock = os.open(dist, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
                cleanup.callback(os.close, lock)
                fcntl.flock(lock, fcntl.LOCK_EX)
            check_outputs(dist)
            install_outputs(dist, work, archive_path)


if __name__ == "__main__":
    package(Path(__file__).resolve().parent.parent)
    print("Local ad-hoc macOS arm64 candidate packaged.")

"""Local package validation. Run with python3 and python3 -O."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile


sys.dont_write_bytecode = True
script = Path(__file__).resolve().parents[1] / "scripts/package-macos.py"
spec = importlib.util.spec_from_file_location("workflow_mac_package", script)
packager = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packager)


class PackageTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="workflow-mac-package-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.app = self.root / packager.APP
        self.dist = self.root / "dist"
        self.dist.mkdir()
        self.outputs = {name: ("previous " + name).encode("ascii") for name in
                        (packager.ZIP_NAME, packager.SUM_NAME, packager.FEED_NAME)}
        for name, contents in self.outputs.items():
            (self.dist / name).write_bytes(contents)
        self.write("tauri.conf.json", json.dumps({"version": "1.2.3"}).encode())
        marker = b"synthetic public marker"
        self.write("src/update-public-key.txt", marker)
        self.icon = b"synthetic package icon"
        self.write("target/macos-icons/Workflow-Mac.icns", self.icon)
        for name, source in packager.DOCUMENTS.items():
            self.write(source, name.encode())
        metadata = {"CFBundleIdentifier": "local.workflow.desktop", "CFBundleVersion": "1.2.3",
                    "CFBundleShortVersionString": "1.2.3", "CFBundleExecutable": "workflow",
                    "CFBundleIconFile": "Workflow-Mac.icns"}
        self.write(packager.APP + "/" + packager.PLIST, plistlib.dumps(metadata, fmt=plistlib.FMT_BINARY))
        self.write(packager.APP + "/" + packager.ICON, self.icon)
        self.write(packager.APP + "/" + packager.SIGNATURE, b"synthetic signature resource")
        binary = bytearray(32)
        binary[:8] = bytes.fromhex("cffaedfe0c000001")
        binary[12] = 2
        self.binary = bytes(binary) + marker
        self.exe = self.write(packager.APP + "/Contents/MacOS/workflow", self.binary)
        self.exe.chmod(0o755)

    def write(self, name, content):
        file = self.root / name
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_bytes(content)
        return file

    def codesign(self, args, **kwargs):
        self.assertEqual(args[0], "/usr/bin/codesign")
        self.assertTrue(kwargs["check"])
        self.assertTrue(kwargs["capture_output"])
        return subprocess.CompletedProcess(args, 0, stderr=b"Signature=adhoc\n")

    def preserved(self):
        self.assertEqual({p.name for p in self.dist.iterdir()}, set(self.outputs))
        for name, contents in self.outputs.items():
            self.assertEqual((self.dist / name).read_bytes(), contents)

    def make_directory_link(self, link, target):
        try:
            link.symlink_to(target, target_is_directory=True)
        except OSError:
            if os.name != "nt":
                raise
            subprocess.run(["cmd.exe", "/d", "/c", "mklink", "/J", str(link), str(target)],
                           check=True, capture_output=True)

    def test_success_packages_exact_resources_and_replaces_matching_outputs(self):
        with patch.object(packager.subprocess, "run", side_effect=self.codesign):
            packager.package(self.root)
        self.assertEqual({p.name for p in self.dist.iterdir()}, {packager.ZIP_NAME, packager.SUM_NAME})
        archive = self.dist / packager.ZIP_NAME
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        self.assertEqual((self.dist / packager.SUM_NAME).read_text(), digest + "  " + archive.name + "\n")
        with zipfile.ZipFile(archive) as package:
            prefix = "Workflow.app/"
            expected = {packager.PLIST, packager.ICON, packager.SIGNATURE, "Contents/MacOS/workflow"}
            for name in packager.DOCUMENTS:
                member = "Contents/Resources/" + name
                expected.add(member)
                self.assertEqual(package.read(prefix + member), name.encode())
            self.assertEqual({entry.filename[len(prefix):] for entry in package.infolist()
                              if not entry.is_dir()}, expected)
            self.assertEqual(package.read(prefix + packager.ICON), self.icon)
            self.assertEqual(package.read(prefix + "Contents/MacOS/workflow"), self.binary)
            self.assertTrue(package.read(prefix + packager.PLIST).startswith(b"<?xml"))

    def test_binary_icon_and_private_path_validation_preserve_existing_outputs(self):
        private_path = "/".join(["", "Users", "synthetic", "file"]).encode()
        for extra in (b"test-control.json", private_path, private_path.decode().encode("utf-16le")):
            with self.subTest(category="binary"), patch.object(packager.subprocess, "run") as codesign:
                self.exe.write_bytes(self.binary + extra)
                with self.assertRaises(ValueError):
                    packager.package(self.root)
                codesign.assert_not_called()
                self.preserved()
        self.exe.write_bytes(self.binary)
        (self.app / packager.ICON).write_bytes(b"different synthetic icon")
        with patch.object(packager.subprocess, "run") as codesign, self.assertRaises(ValueError):
            packager.package(self.root)
        codesign.assert_not_called()
        self.preserved()

    def test_unlisted_empty_directory_fails_before_staging(self):
        (self.app / "Contents/Resources/unlisted").mkdir()
        with patch.object(packager.tempfile, "TemporaryDirectory") as staging, self.assertRaises(ValueError):
            packager.package(self.root)
        staging.assert_not_called()
        self.preserved()

    def test_linked_resources_and_support_ancestors_fail_before_staging(self):
        for path in (self.app / "Contents/Resources", self.root / "ui/vendor"):
            with self.subTest(category=path.name):
                original = path.with_name(path.name + "-outside")
                path.rename(original)
                self.make_directory_link(path, original)
                try:
                    with patch.object(packager.tempfile, "TemporaryDirectory") as staging, self.assertRaises(ValueError):
                        packager.package(self.root)
                    staging.assert_not_called()
                    self.preserved()
                finally:
                    if path.is_symlink():
                        path.unlink()
                    else:
                        path.rmdir()  # Windows directory junction.
                    original.rename(path)

    def test_special_support_input_fails_before_open(self):
        support = self.root / packager.DOCUMENTS["THIRD-PARTY-NOTICES.txt"]
        support.unlink()
        if hasattr(os, "mkfifo"):
            os.mkfifo(support)
        else:
            support.mkdir()  # Windows has no POSIX FIFO.
        with patch.object(packager.tempfile, "TemporaryDirectory") as staging, self.assertRaises(ValueError):
            packager.package(self.root)
        staging.assert_not_called()
        self.preserved()

    def test_changed_staged_resource_directory_is_rejected_before_writing(self):
        outside = self.root / "outside"
        outside.mkdir()
        sentinel = outside / "LICENSE.txt"
        sentinel.write_bytes(b"synthetic outside file")
        def change_staged(args, **kwargs):
            if "--display" in args:
                resources = Path(args[-1]) / "Contents/Resources"
                shutil.rmtree(resources)
                self.make_directory_link(resources, outside)
            return self.codesign(args, **kwargs)
        # Keep the real subprocess.run for creating a Windows junction inside the mock.
        real_run = subprocess.run
        def run(args, **kwargs):
            if args[0] != "/usr/bin/codesign":
                return real_run(args, **kwargs)
            return change_staged(args, **kwargs)
        with patch.object(packager.subprocess, "run", side_effect=run) as codesign, self.assertRaises(ValueError):
            packager.package(self.root)
        self.assertTrue(all("--force" not in call.args[0] for call in codesign.call_args_list))
        self.assertEqual(sentinel.read_bytes(), b"synthetic outside file")
        self.preserved()

    def test_signed_bundle_is_rechecked_before_creating_archive(self):
        def changed_tree(args, **kwargs):
            if "--force" in args:
                (Path(args[-1]) / "Contents/Resources/unexpected").write_bytes(b"synthetic extra")
            return self.codesign(args, **kwargs)
        with patch.object(packager.subprocess, "run", side_effect=changed_tree), self.assertRaises(ValueError):
            packager.package(self.root)
        self.preserved()

    def test_archive_write_failure_preserves_previous_zip_checksum_and_feed(self):
        with patch.object(packager.subprocess, "run", side_effect=self.codesign), \
                patch.object(zipfile.ZipFile, "write", side_effect=OSError("synthetic ZIP failure")), \
                self.assertRaises(OSError):
            packager.package(self.root)
        self.preserved()

    def test_checksum_replace_failure_rolls_back_all_previous_outputs(self):
        original_replace = os.replace
        def replace(source, destination):
            if source.name == packager.SUM_NAME and source.parent != self.dist:
                raise OSError("synthetic checksum publication failure")
            return original_replace(source, destination)
        with patch.object(packager.subprocess, "run", side_effect=self.codesign), \
                patch.object(os, "replace", side_effect=replace), self.assertRaises(OSError):
            packager.package(self.root)
        self.preserved()


if __name__ == "__main__":
    unittest.main()

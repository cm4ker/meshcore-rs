import hashlib
import json
import struct
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

import firmware_library as library


TAG = "v0.9.0-r194"
PREFIX = "t3s3-companion-0.9.0-r194"
OLD_IMAGE = b"running application"
NEW_IMAGE = b"new application"


def image_file(delta=False):
    header = (struct.pack("<4sBBBBII", b"MCFW", 1, 8, 2, 0, 194, len(NEW_IMAGE)) +
              hashlib.sha256(NEW_IMAGE).digest() + bytes(5) + b"\x01" + bytes(96))
    if delta:
        return header + struct.pack("<I", len(OLD_IMAGE)) + hashlib.sha256(OLD_IMAGE).digest() + b"patch"
    return header + NEW_IMAGE


def asset(name, data):
    return dict(name=name, size=len(data), digest="sha256:" + hashlib.sha256(data).hexdigest(),
                browser_download_url=f"https://github.com/cm4ker/meshcore-rs/releases/download/{TAG}/{name}")


class FirmwareLibraryTest(unittest.TestCase):
    def setUp(self):
        self.temporary = TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.full, self.delta = PREFIX + ".mcfw", PREFIX + ".from-r192.mcfd"
        self.images = {self.full: image_file(), self.delta: image_file(delta=True)}
        self.data = [dict(tag_name=TAG, assets=[asset(name, data) for name, data in self.images.items()])]
        self.device = dict(board=8, role=2, build=192, capacity=1024, base_hash=hashlib.sha256(OLD_IMAGE).hexdigest()[:16],
                           running_size=len(OLD_IMAGE), summary="LilyGO T3-S3")

    def load(self):
        with patch.object(library, "fetch", return_value=json.dumps(self.data).encode()):
            return library.index(self.root)

    def fetch(self, url, **options):
        return self.images[url.rsplit("/", 1)[-1]]

    def test_resume_skips_verified_files_and_replaces_corruption(self):
        self.load()
        requested = []

        def interrupted(url, **options):
            requested.append(url)
            if url.endswith(".mcfd"):
                raise OSError("no connection")
            return self.fetch(url, **options)

        with patch("firmware_catalog.fetch", side_effect=interrupted), self.assertRaisesRegex(ValueError, "1 из 2"):
            library.sync(TAG, 8, 2, self.root)
        self.assertEqual(len(requested), 2)
        stored = library.saved({}, self.root)["files"]
        self.assertEqual([row["name"] for row in stored], [self.full])
        with patch("firmware_catalog.fetch", side_effect=self.fetch) as download:
            self.assertEqual(library.sync(TAG, 8, 2, self.root), dict(downloaded=1, existing=1))
            self.assertEqual(download.call_count, 1)
            self.assertTrue(download.call_args.args[0].endswith(".mcfd"))
            path = Path(stored[0]["path"])
            path.write_bytes(bytes(path.stat().st_size))
            with self.assertRaisesRegex(ValueError, "повреждён"):
                library.verify(path, stored[0]["digest"], self.root)
            self.assertEqual(library.sync(TAG, 8, 2, self.root), dict(downloaded=1, existing=1))

    def test_library_checks_base_hash_without_network(self):
        self.load()
        with patch("firmware_catalog.fetch", side_effect=self.fetch):
            library.sync(TAG, 0, 0, self.root)
        with patch("firmware_catalog.fetch", side_effect=AssertionError("offline selection contacted GitHub")):
            for digest, name in ((self.device["base_hash"], self.delta), ("e" * 16, self.full)):
                found = library.saved(dict(device=dict(self.device, base_hash=digest)), self.root)
                self.assertEqual([row["name"] for row in found["files"] if row["recommended"]], [name])
                row = next(row for row in found["files"] if row["recommended"])
                self.assertEqual(library.verify(row["path"], row["digest"], self.root).read_bytes(), self.images[name])
                label = "Дельта .mcfd · r192 → r194" if name == self.delta else "Полная .mcfw · r194"
                self.assertIn(label, row["label"])
            self.assertFalse(next(row for row in found["files"] if row["name"] == self.delta)["enabled"])

    def test_preexisting_download_is_visible_and_missing_metadata_is_ignored(self):
        legacy = self.root / "release-before-library"
        legacy.mkdir()
        (legacy / self.full).write_bytes(self.images[self.full])
        folder = self.root / "library" / "release-interrupted"
        folder.mkdir(parents=True)
        (folder / self.delta).write_bytes(self.images[self.delta])
        records = library.saved(dict(formats=[".mcfw", ".mcfd"]), self.root)
        self.assertEqual(records["total"], 1)
        self.assertEqual(records["files"][0]["tag"], "r194")

    def test_bundle_excludes_other_assets_and_can_select_one_model_and_role(self):
        unsafe = self.data[0]["assets"] + [
            asset("hashes.txt", b"text"), asset("heltec-v4-repeater.uf2", bytes(512)),
        ]
        versions = library.build_index([dict(tag_name=TAG, assets=unsafe)])
        self.assertEqual({a["name"] for a in versions[0]["assets"]}, {self.full, self.delta})
        self.load()
        with self.assertRaisesRegex(ValueError, "нет файлов"):
            library.sync(TAG, 7, 2, self.root)


if __name__ == "__main__":
    unittest.main()

"""package.py keeps every manifest it writes under a tag by digest as well
(docs/decisions/0006-manifest-retention.md); no network, weights or model runtimes needed."""
import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from ollaya_convert import package


CONFIG = {"mediaType": package.MEDIA["config"], "digest": "sha256:" + "0" * 64, "size": 2}


class ManifestRetention(unittest.TestCase):
    def test_a_tag_is_also_kept_by_digest_and_moving_it_keeps_the_old_one(self):
        with tempfile.TemporaryDirectory() as root, patch.object(package, "REGISTRY", root):
            manifests = Path(root, "v2", "library", "m", "manifests")
            digests = []
            for size in (1, 2):
                layer = dict(CONFIG, mediaType=package.MEDIA["license"], size=size)
                package.write_manifest("library", "m", "en", CONFIG, [layer])
                body = (manifests / "en").read_bytes()
                digest = hashlib.sha256(body).hexdigest()
                self.assertEqual((manifests / ("sha256-" + digest)).read_bytes(), body)
                self.assertEqual(json.loads(body)["layers"], [layer])
                digests.append(digest)
            self.assertNotEqual(digests[0], digests[1])
            self.assertEqual(sorted(p.name for p in manifests.iterdir()),
                             sorted(["en"] + ["sha256-" + d for d in digests]))

    def test_a_tag_can_not_look_like_a_digest(self):
        with tempfile.TemporaryDirectory() as root, patch.object(package, "REGISTRY", root):
            with self.assertRaises(SystemExit):
                package.write_manifest("library", "m", "sha256-" + "0" * 64, CONFIG, [])
            self.assertFalse(Path(root, "v2").exists())

    def test_the_committed_registry_keeps_every_tag_by_digest(self):
        registry = Path(package.REPO_ROOT, "registry", "v2")
        tags = [p for p in registry.glob("*/*/manifests/*") if not p.name.startswith("sha256-")]
        self.assertTrue(tags)
        for tag in tags:
            body = tag.read_bytes()
            kept = tag.with_name("sha256-" + hashlib.sha256(body).hexdigest())
            self.assertTrue(kept.is_file(), "%s has no copy by digest" % tag)
            self.assertEqual(kept.read_bytes(), body)


if __name__ == "__main__":
    unittest.main()

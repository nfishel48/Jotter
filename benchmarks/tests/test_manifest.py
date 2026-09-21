"""Round-trip tests for the files that join a corpus to a score."""

import json
import tempfile
import unittest
from pathlib import Path

from jbench import manifest


class TestManifest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.items_path = self.tmp / "items.jsonl"
        self.refs_path = self.tmp / "references.jsonl"

    def test_round_trip(self):
        items = [
            manifest.Item(id="a", audio=self.tmp / "a.wav", reference="hello"),
            manifest.Item(id="b", audio=self.tmp / "b.wav", reference="world", speaker="s1"),
        ]
        self.assertEqual(manifest.write(items, self.items_path, self.refs_path), 2)

        references = manifest.read_references(self.refs_path)
        self.assertEqual(references["a"]["reference"], "hello")
        self.assertEqual(references["b"]["speaker"], "s1")

    def test_audio_paths_are_absolute(self):
        # The bench binary is run from wherever the user happens to be; a
        # relative path resolves against the wrong directory and looks exactly
        # like a missing corpus.
        manifest.write(
            [manifest.Item(id="a", audio=Path("relative.wav"), reference="x")],
            self.items_path,
            self.refs_path,
        )
        row = json.loads(self.items_path.read_text().strip())
        self.assertTrue(Path(row["audio"]).is_absolute())

    def test_hypotheses_without_provenance_are_rejected(self):
        # Scoring a file whose model and build are unknown produces a number
        # that cannot be reproduced. Better to refuse than to publish it.
        path = self.tmp / "hyps.jsonl"
        path.write_text(json.dumps({"record": "item", "id": "a", "text": "hi"}) + "\n")
        with self.assertRaises(ValueError):
            manifest.read_hypotheses(path)

    def test_provenance_is_split_from_the_items(self):
        path = self.tmp / "hyps.jsonl"
        path.write_text(
            json.dumps({"record": "provenance", "model_id": "m", "segmentation": "vad"}) + "\n"
            + json.dumps({"record": "item", "id": "a", "text": "hi"}) + "\n"
        )
        provenance, items = manifest.read_hypotheses(path)
        self.assertEqual(provenance["model_id"], "m")
        self.assertEqual(set(items), {"a"})

    def test_a_corrupt_line_names_its_line_number(self):
        path = self.tmp / "hyps.jsonl"
        path.write_text('{"record": "provenance"}\nnot json\n')
        with self.assertRaises(ValueError) as caught:
            manifest.read_hypotheses(path)
        self.assertIn(":2", str(caught.exception))


if __name__ == "__main__":
    unittest.main()

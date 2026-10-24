#!/usr/bin/env python3
"""Unit tests for CI support scripts (generate_review_prompt.py and patch_diffguard.py)."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))

import generate_review_prompt as grp  # noqa: E402
import patch_diffguard as pdg  # noqa: E402


class TestGenerateReviewPrompt(unittest.TestCase):
    """Tests fence-tracked markdown transformations and error detection."""

    def test_unbalanced_fence_raises(self) -> None:
        with self.assertRaisesRegex(ValueError, "unbalanced code fences"):
            list(grp.iter_fence_tracked_lines("```rust\nlet x = 1;\n"))

    def test_inner_fence_with_info_string_does_not_close_outer_fence(self) -> None:
        md = "```markdown\n```rust\nlet x = 1;\n```\n"
        events = list(grp.iter_fence_tracked_lines(md))
        self.assertEqual(len(events), 4)
        # Line 0 opens outer fence
        self.assertEqual(events[0][:2], (True, True))
        # Line 1 has info string 'rust', so CommonMark treats it as content inside the open fence
        self.assertEqual(events[1][:2], (False, True))
        # Line 3 has no info string, closing the fence
        self.assertEqual(events[3][:2], (True, False))

    def test_unterminated_html_comment_raises(self) -> None:
        with self.assertRaisesRegex(ValueError, "unterminated HTML comment"):
            grp.strip_decorations_outside_fences("Hello <!-- unclosed\nstill open\n")

    def test_preserves_comments_and_links_inside_fences(self) -> None:
        md = (
            "Outside <!-- stripped --> [link](https://example.com) {#anchor}\n"
            "```rust\n"
            "// <!-- preserved --> [link](https://example.com) {#anchor}\n"
            "```\n"
        )
        result = grp.strip_decorations_outside_fences(md)
        self.assertIn("Outside  link", result)
        self.assertNotIn("<!-- stripped -->", result)
        self.assertIn("// <!-- preserved --> [link](https://example.com) {#anchor}", result)

    def test_split_top_level_sections_ignores_headers_in_fences(self) -> None:
        md = "# Section One\n```sh\n# Inside Fence\n```\n# Section Two\nBody\n"
        sections = grp.split_top_level_sections(md)
        self.assertEqual(len(sections), 2)
        self.assertIn("# Inside Fence", sections[0])
        self.assertTrue(sections[1].startswith("# Section Two"))


class TestPatchDiffguard(unittest.TestCase):
    """Tests idempotent action-bundle patching, syntax checks, and failure modes."""

    def _create_bundle(self, root: Path, content: str) -> Path:
        bundle = root / "jonit-dev" / "diffguard" / "v1" / "dist" / "index.js"
        bundle.parent.mkdir(parents=True, exist_ok=True)
        bundle.write_text(content, encoding="utf-8")
        return bundle

    def test_patches_unpatched_bundle_and_is_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bundle = self._create_bundle(
                root,
                "const core = { getInput: () => '', setFailed: () => {} };\n"
                "const customPrompt = core.getInput('custom_prompt');\n",
            )
            with patch("patch_diffguard.subprocess.run") as mock_run:
                # First run applies the patch
                pdg.patch_bundle_in_roots([str(root)])
                patched_text = bundle.read_text(encoding="utf-8")
                self.assertIn("_fs.readFileSync(customPrompt, 'utf8')", patched_text)
                self.assertNotIn("const customPrompt = core.getInput('custom_prompt');", patched_text)
                mock_run.assert_called_with(
                    ["node", "--check", str(bundle)],
                    check=True,
                    capture_output=True,
                    text=True,
                    timeout=10,
                )

                # Second run succeeds idempotently
                pdg.patch_bundle_in_roots([str(root)])
                self.assertEqual(mock_run.call_count, 2)

    def test_verify_js_syntax_handles_timeout_and_errors(self) -> None:
        with patch(
            "patch_diffguard.subprocess.run",
            side_effect=subprocess.TimeoutExpired(cmd=["node", "--check"], timeout=10),
        ):
            with self.assertRaises(SystemExit) as ctx:
                pdg.verify_js_syntax("dummy.js")
            self.assertEqual(ctx.exception.code, 1)

        with patch("patch_diffguard.subprocess.run", side_effect=FileNotFoundError()):
            with self.assertRaises(SystemExit) as ctx:
                pdg.verify_js_syntax("dummy.js")
            self.assertEqual(ctx.exception.code, 1)

    def test_rejects_half_patched_bundle(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._create_bundle(
                root,
                "const customPrompt = core.getInput('custom_prompt');\n"
                "// _fs.readFileSync(customPrompt, 'utf8')\n",
            )
            with self.assertRaises(SystemExit) as ctx:
                pdg.patch_bundle_in_roots([str(root)])
            self.assertEqual(ctx.exception.code, 1)

    def test_rejects_missing_bundle(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(SystemExit) as ctx:
                pdg.patch_bundle_in_roots([tmp])
            self.assertEqual(ctx.exception.code, 1)


if __name__ == "__main__":
    unittest.main()

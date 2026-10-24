#!/usr/bin/env python3
"""Unit tests for CI support scripts (generate_review_prompt.py and patch_diffguard.py).

Requires Python >= 3.10.
"""

from pathlib import Path
import os
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

    def test_four_space_indented_backticks_are_not_treated_as_fence(self) -> None:
        md = "    ```rust\n    indented code block\n"
        events = list(grp.iter_fence_tracked_lines(md))
        self.assertEqual(events, [(False, False, "    ```rust\n"), (False, False, "    indented code block\n")])

    def test_inner_fence_with_info_string_does_not_close_outer_fence(self) -> None:
        md = "```markdown\n```rust\nlet x = 1;\n```\n"
        events = list(grp.iter_fence_tracked_lines(md))
        self.assertEqual(len(events), 4)
        self.assertEqual(events[0][:2], (True, True))
        self.assertEqual(events[1][:2], (False, True))
        self.assertEqual(events[3][:2], (True, False))

    def test_unterminated_html_comment_raises(self) -> None:
        with self.assertRaisesRegex(ValueError, "unterminated HTML comment"):
            grp.strip_decorations_outside_fences("Hello <!-- unclosed\nstill open\n")

    def test_unterminated_html_comment_before_fence_raises(self) -> None:
        md = "<!-- unclosed comment\n```rust\nfn main() {}\n```\n"
        with self.assertRaisesRegex(ValueError, "unterminated HTML comment before code fence"):
            grp.strip_decorations_outside_fences(md)

    def test_stray_close_arrow_before_open_comment_handled_cleanly(self) -> None:
        md = "a --> b <!-- multiline\ncomment --> c\n"
        self.assertEqual(grp.strip_decorations_outside_fences(md), "a --> b  c")

        with self.assertRaisesRegex(ValueError, "unterminated HTML comment"):
            grp.strip_decorations_outside_fences("a --> b <!-- unclosed\n")

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

    def test_load_file_missing_raises(self) -> None:
        with self.assertRaises(FileNotFoundError):
            grp.load_file(Path("/nonexistent/path/to/guidelines.md"))

    def test_build_review_prompt_on_repo_and_budget_overflow(self) -> None:
        repo_root = Path(__file__).resolve().parents[2]
        prompt = grp.build_review_prompt(repo_root)
        self.assertLessEqual(len(prompt.encode("utf-8")), grp.MAX_PROMPT_BYTES)
        self.assertIn("# SECTION 1: REPOSITORY ROOT RULES", prompt)
        self.assertIn("# SECTION 3: MICROSOFT PRAGMATIC RUST GUIDELINES", prompt)
        self.assertNotIn("# FFI Guidelines", prompt)
        self.assertNotIn("# Macros Guidelines", prompt)

        with patch.object(grp, "MAX_PROMPT_BYTES", 100):
            with self.assertRaises(SystemExit):
                grp.build_review_prompt(repo_root)

    def test_generate_review_prompt_main_github_output(self) -> None:
        repo_root = Path(__file__).resolve().parents[2]
        prompt_path = repo_root / grp.PROMPT_OUTPUT_FILENAME
        prompt_path.unlink(missing_ok=True)
        try:
            with tempfile.TemporaryDirectory() as tmp:
                output_file = Path(tmp) / "github_output.txt"
                with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output_file)}):
                    grp.main()
                    self.assertTrue(output_file.exists())
                    content = output_file.read_text(encoding="utf-8")
                    self.assertIn("prompt_file=", content)
                    out_path = Path(content.strip().split("=", 1)[1])
                    self.assertTrue(out_path.is_file())
                    self.assertGreater(out_path.stat().st_size, 0)
        finally:
            prompt_path.unlink(missing_ok=True)

    def test_generate_review_prompt_main_without_github_output(self) -> None:
        repo_root = Path(__file__).resolve().parents[2]
        prompt_path = repo_root / grp.PROMPT_OUTPUT_FILENAME
        prompt_path.unlink(missing_ok=True)
        try:
            with patch.dict(os.environ, {}, clear=True):
                grp.main()
                self.assertTrue(prompt_path.is_file())
                self.assertGreater(prompt_path.stat().st_size, 0)
        finally:
            prompt_path.unlink(missing_ok=True)


class TestPatchDiffguard(unittest.TestCase):
    """Tests idempotent action-bundle patching, syntax checks, and failure modes."""

    def _create_bundle(self, root: Path, content: str, sha: str = pdg.PINNED_SHA) -> Path:
        bundle = root / "jonit-dev" / "diffguard" / sha / "dist" / "index.js"
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
                pdg.patch_bundle_in_roots([str(root)])
                patched_text = bundle.read_text(encoding="utf-8")
                self.assertIn(pdg.PATCH_MARKER, patched_text)
                self.assertNotIn(pdg.TARGET_LINE, patched_text)
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

    def test_rejects_mismatched_sha_half_patched_zero_or_multi_target(self) -> None:
        # Mismatched SHA path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._create_bundle(root, pdg.TARGET_LINE, sha="deadbeef")
            with self.assertRaises(SystemExit) as ctx:
                pdg.patch_bundle_in_roots([str(root)])
            self.assertEqual(ctx.exception.code, 1)

        # Half-patched bundle
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._create_bundle(root, f"{pdg.TARGET_LINE}\n// {pdg.PATCH_MARKER}\n")
            with self.assertRaises(SystemExit) as ctx:
                pdg.patch_bundle_in_roots([str(root)])
            self.assertEqual(ctx.exception.code, 1)

        # Zero occurrences of target in matched bundle
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._create_bundle(root, "console.log('unrelated bundle');\n")
            with self.assertRaises(SystemExit) as ctx:
                pdg.patch_bundle_in_roots([str(root)])
            self.assertEqual(ctx.exception.code, 1)

        # Multiple occurrences of target in matched bundle
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._create_bundle(root, f"{pdg.TARGET_LINE}\n{pdg.TARGET_LINE}\n")
            with self.assertRaises(SystemExit) as ctx:
                pdg.patch_bundle_in_roots([str(root)])
            self.assertEqual(ctx.exception.code, 1)

    def test_rejects_missing_bundle(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(SystemExit) as ctx:
                pdg.patch_bundle_in_roots([tmp])
            self.assertEqual(ctx.exception.code, 1)

    def test_verify_js_syntax_handles_timeout_and_errors(self) -> None:
        for side_effect in (
            subprocess.TimeoutExpired(cmd=["node", "--check"], timeout=10),
            FileNotFoundError(),
            subprocess.CalledProcessError(returncode=1, cmd=["node", "--check"], stderr="SyntaxError"),
        ):
            with patch("patch_diffguard.subprocess.run", side_effect=side_effect):
                with self.assertRaises(SystemExit) as ctx:
                    pdg.verify_js_syntax("dummy.js")
                self.assertEqual(ctx.exception.code, 1)


if __name__ == "__main__":
    unittest.main()

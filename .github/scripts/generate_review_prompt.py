#!/usr/bin/env python3
"""Generates the full custom review prompt for DiffGuard AI PR Review.

Reads repository guidelines directly from:
- AGENTS.md (Root architecture, target hardware, safety rules, coverage)
- services/core/AGENTS.md (Hardware abstraction, inherent mocking, domain types)
- services/.guidelines/microsoft-rust-guidelines.md (Microsoft Pragmatic Rust rules)

Formats all rules into a comprehensive review prompt and writes it directly to
`review_prompt.txt` at the repository root, exporting its file path to GITHUB_OUTPUT
to completely bypass the Linux MAX_ARG_STRLEN (128 KB) environment variable limit.
"""

import os
import re
from pathlib import Path

# Prompt file output path at repo root
PROMPT_OUTPUT_FILENAME = "review_prompt.txt"
FENCE_RE = re.compile(r"^\s*(`{3,}|~{3,})")


def load_file(path: Path) -> str:
    """Reads file text from repository path."""
    if not path.is_file():
        raise FileNotFoundError(f"Guidelines file not found at: {path}")
    return path.read_text(encoding="utf-8").strip()


def split_top_level_sections(md: str) -> list[str]:
    """Splits markdown into top-level sections (# Title), ignoring headers inside code fences."""
    lines = md.splitlines(keepends=True)
    sections = []
    current_section = []
    current_fence = None

    for line in lines:
        m = FENCE_RE.match(line)
        if m:
            fence_chars = m.group(1)
            if current_fence is None:
                current_fence = fence_chars
            elif fence_chars[0] == current_fence[0] and len(fence_chars) >= len(current_fence):
                current_fence = None
            current_section.append(line)
            continue

        if current_fence is None and re.match(r"^# [A-Z]", line) and current_section:
            sections.append("".join(current_section))
            current_section = [line]
        else:
            current_section.append(line)

    if current_fence is not None:
        raise ValueError(
            f"unbalanced code fences detected in markdown file: fence {current_fence} was not closed"
        )

    if current_section:
        sections.append("".join(current_section))
    return sections


def strip_decorations_outside_fences(md: str) -> str:
    """Strips anchor tags and link URLs outside of code fences, preserving code blocks."""
    lines = md.splitlines(keepends=True)
    out: list[str] = []
    current_fence = None
    in_html_comment = False
    consecutive_blank_lines = 0

    for line in lines:
        if current_fence is None and in_html_comment:
            if "-->" in line:
                in_html_comment = False
                line = line.split("-->", 1)[1]
            else:
                continue

        m = FENCE_RE.match(line)
        if m:
            marker = m.group(1)
            if current_fence is None:
                current_fence = marker
            elif marker[0] == current_fence[0] and len(marker) >= len(current_fence):
                current_fence = None
            out.append(line)
            consecutive_blank_lines = 0
            continue

        if current_fence is not None:
            # Code block content: preserve exactly as authored
            out.append(line)
            consecutive_blank_lines = 0
            continue

        # Outside fences: handle HTML comments without crossing fence boundaries
        while "<!--" in line:
            if "-->" in line:
                before, rest = line.split("<!--", 1)
                _, after = rest.split("-->", 1)
                line = before + after
            else:
                line = line.split("<!--", 1)[0]
                in_html_comment = True
                break

        line = re.sub(r"^\s*\[[^\]]+\]:\s+\S+.*$", "", line)
        line = re.sub(r"\s*\{\s*#[A-Za-z0-9\-_]+\s*\}", "", line)
        line = re.sub(r"\[([^\]]+)\]\([^\)]+\)", r"\1", line)

        # Collapse excessive blank lines outside fences
        if not line.strip():
            consecutive_blank_lines += 1
            if consecutive_blank_lines > 1:
                continue
            out.append("\n")
        else:
            consecutive_blank_lines = 0
            out.append(line)

    return "".join(out).strip()


def build_review_prompt(repo_root: Path) -> str:
    """Assembles the complete PR review prompt containing the full guidelines."""
    agents_path = repo_root / "AGENTS.md"
    core_agents_path = repo_root / "services" / "core" / "AGENTS.md"
    guidelines_path = repo_root / "services" / ".guidelines" / "microsoft-rust-guidelines.md"

    agents_content = load_file(agents_path)
    core_agents_content = load_file(core_agents_path)
    raw_guidelines = load_file(guidelines_path)

    # Process guidelines:
    # 1. Filter sections that are irrelevant to native Rust code reviews (e.g. C-FFI Windows DLLs,
    #    proc-macro crate development, and meta AI-assistant prompt instructions).
    sections = split_top_level_sections(raw_guidelines)
    kept_sections = []
    excluded_keywords = [
        "AI Guidelines",
        "FFI Guidelines",
        "Macros Guidelines",
    ]

    for section in sections:
        header = next((line for line in section.splitlines() if line.startswith("# ")), "")
        if any(keyword in header for keyword in excluded_keywords):
            continue
        kept_sections.append(section)

    guidelines_body = "\n".join(kept_sections)
    guidelines_body = strip_decorations_outside_fences(guidelines_body)

    prompt = f"""You are reviewing pull requests for the Raspberry Pi Bluetooth Speaker & DSP Appliance project.
Perform a rigorous, constructive, and actionable engineering review adhering strictly to the guidelines and standards below.

# SECTION 1: REPOSITORY ROOT RULES (AGENTS.md)
{agents_content}

# SECTION 2: CORE SERVICE & HARDWARE RULES (services/core/AGENTS.md)
{core_agents_content}

# SECTION 3: MICROSOFT PRAGMATIC RUST GUIDELINES (FULL RULES & BEST PRACTICES)
{guidelines_body}

# REVIEW INSTRUCTIONS & REQUIRED OUTPUT FORMAT:
- Analyze all diffs thoroughly against the above standards.
- Point out concrete issues with exact file paths and line numbers.
- Avoid generic advice or low-value style nitpicks; focus on actionable defects, invariant violations, unsafe patterns, compliance gaps, and concrete code simplifications.
- Explicitly evaluate how the code can be simplified: identify duplicated scaffolding across binaries/tests that can be unified, over-engineered abstractions or redundant state that can be removed, and control flow or error handling that can be flattened.
- Structure your review using the following sections:
  1. Brief Summary of changes
  2. Strengths / Good patterns observed
  3. Critical Issues (bugs, invariant violations, unhandled errors, panics, boundary issues)
  4. Compliance Findings (Pragmatic Rust Guidelines, Target Architecture Cortex-A53 / NEON, Code Coverage >85-90%+)
  5. Code Simplification & Deduplication Opportunities (concrete ways to reduce complexity, eliminate duplication, and shrink surface area)
  6. Actionable Recommendations with code snippets where helpful
"""
    byte_len = len(prompt.encode("utf-8"))
    print(
        f"Generated review prompt size: {byte_len} bytes ({byte_len / 1024:.2f} KB) across {len(kept_sections)} guideline sections"
    )

    return prompt


def main() -> None:
    # Resolve repository root
    script_dir = Path(__file__).resolve().parent
    repo_root = script_dir.parent.parent

    prompt = build_review_prompt(repo_root)
    byte_len = len(prompt.encode("utf-8"))

    # Enforce model context budget for OpenRouter @preset/code-review (~128k token context window)
    MAX_PROMPT_BYTES = 200_000
    if byte_len > MAX_PROMPT_BYTES:
        raise SystemExit(
            f"ERROR: review prompt size ({byte_len} bytes) exceeds configured model context budget ({MAX_PROMPT_BYTES} bytes)"
        )

    # Save to external file at repo root so action can read it directly without environment limits
    prompt_file = repo_root / PROMPT_OUTPUT_FILENAME
    prompt_file.write_text(prompt, encoding="utf-8")
    print(f"Successfully saved full review prompt to file: {prompt_file}")

    github_output = os.environ.get("GITHUB_OUTPUT")
    if github_output:
        with open(github_output, "a", encoding="utf-8") as out:
            out.write(f"prompt_file={prompt_file.resolve()}\n")
        print("Successfully exported 'prompt_file' to GITHUB_OUTPUT.")
    else:
        print("GITHUB_OUTPUT environment variable not set (running locally).")


if __name__ == "__main__":
    main()

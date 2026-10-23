"""Синтетические фикстуры для проверки границы пользовательских данных."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from check_local_data_untracked import (
    ignore_rule_problems,
    tracked_paths,
    tracked_user_data,
)

GITIGNORE = """
# Пользовательские данные никогда не публикуются.
/decks/
/.asset-store/
"""


class TrackedUserDataTests(unittest.TestCase):
    def test_user_areas_are_reported(self):
        tracked = [
            "decks/input/Hidden/deck.json",
            "decks/output/Words__N5/deck.json",
            "decks/output/Words__N5/media/word.mp3",
            ".asset-store/kanji/manifest.json",
            ".asset-store/kanji/assets/png/x.png",
            "decks/deck.json",
            # Любой путь внутри `decks/` — пользовательские данные, а не только
            # две названные подобласти: прежняя раскладка с каталогами
            # группировки тоже не публикуется.
            "decks/legacy/words/Words__N5/deck.json",
            "decks/README.md",
            "decks",
            ".asset-store",
        ]
        self.assertEqual(tracked_user_data(tracked), tracked)

    def test_tooling_paths_are_not_reported(self):
        tracked = [
            "README.md",
            "tools/anki-repo/src/layout.rs",
            "docs/asset-store.md",
            "tools/asset-store/src/store.rs",
        ]
        self.assertEqual(tracked_user_data(tracked), [])

    def test_leading_dot_slash_is_normalized(self):
        self.assertEqual(
            tracked_user_data(["./decks/output/Words__A/deck.json"]),
            ["decks/output/Words__A/deck.json"],
        )

    def test_backslash_separator_is_normalized(self):
        self.assertEqual(
            tracked_user_data(["decks\\input\\Hidden\\deck.json"]),
            ["decks/input/Hidden/deck.json"],
        )


class IgnoreRuleTests(unittest.TestCase):
    def test_complete_rules_pass(self):
        self.assertEqual(ignore_rule_problems(GITIGNORE), [])

    def test_missing_rule_is_reported(self):
        text = "/decks/input/\n/.asset-store/\n"
        problems = ignore_rule_problems(text)
        self.assertTrue(any("/decks/" in problem for problem in problems))

    def test_narrow_rule_does_not_cover_the_whole_area(self):
        text = "/decks/input/\n/decks/output/\n/.asset-store/\n"
        problems = ignore_rule_problems(text)
        self.assertTrue(any("/decks/" in problem for problem in problems))

    def test_nested_re_inclusion_blocked_by_ignored_parent_is_allowed(self):
        text = GITIGNORE + "!/.asset-store/kanji/manifest.json\n"
        self.assertEqual(ignore_rule_problems(text), [])

    def test_unrelated_negation_is_allowed(self):
        text = GITIGNORE + "!*.example\n"
        self.assertEqual(ignore_rule_problems(text), [])

    def test_comment_mentioning_an_area_is_not_a_rule(self):
        text = GITIGNORE + "# !/.asset-store/\n"
        self.assertEqual(ignore_rule_problems(text), [])

    def test_direct_re_inclusion_is_detected_by_git(self):
        for rule in ("!/decks/", "!/.asset-store/"):
            with self.subTest(rule=rule):
                problems = ignore_rule_problems(GITIGNORE + rule + "\n")
                self.assertTrue(
                    any("не игнорируют пользовательский путь" in problem for problem in problems),
                    problems,
                )

    def test_generalized_re_inclusion_is_detected_before_indexing(self):
        problems = ignore_rule_problems(GITIGNORE + "!/deck*/\n")
        self.assertTrue(
            any("decks/output/" in problem for problem in problems),
            problems,
        )

    def test_rules_do_not_hide_regular_tracked_code(self):
        problems = ignore_rule_problems(GITIGNORE + "/*\n")
        self.assertTrue(
            any("ошибочно игнорируют код" in problem for problem in problems),
            problems,
        )


class RepositoryStateTests(unittest.TestCase):
    def test_this_repository_passes_its_own_guard(self):
        root = Path(__file__).resolve().parents[2]
        from check_local_data_untracked import validate_tree

        self.assertEqual(validate_tree(root), [])

    def test_symlinks_named_forbidden_roots_are_detected_in_the_index(self):
        with tempfile.TemporaryDirectory(prefix="anki-tracked-link-") as temporary:
            root = Path(temporary)
            (root / ".gitignore").write_text(GITIGNORE, encoding="utf-8")
            subprocess.run(["git", "init", "--quiet"], cwd=root, check=True)
            target = root / "outside"
            target.mkdir()
            os.symlink(target, root / "decks", target_is_directory=True)
            os.symlink(target, root / ".asset-store", target_is_directory=True)
            subprocess.run(
                ["git", "add", "-f", "--", "decks", ".asset-store"],
                cwd=root,
                check=True,
            )

            tracked = tracked_paths(root)
            self.assertEqual(
                set(tracked_user_data(tracked)), {"decks", ".asset-store"}
            )


if __name__ == "__main__":
    unittest.main()

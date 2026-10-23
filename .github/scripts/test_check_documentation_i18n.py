"""Позитивные и негативные тесты контракта локализации Markdown."""

import tempfile
import unittest
from pathlib import Path

from check_documentation_i18n import (
    git_blob_sha,
    language_navigation,
    translation,
    validate_repository,
)


class DocumentationI18nTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def write(self, relative: str, content: str) -> Path:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        return path

    def group(self, relative: str, target: str = "", shell: str = "echo <имя>") -> Path:
        source = self.write(relative, f"# Название\n\n{language_navigation(self.root / relative, 'ru')}\n\n"
                                     f"{target}\n\n```bash\n{shell}\n```\n")
        digest = git_blob_sha(source.read_bytes())
        for lang in ("en", "zh-CN"):
            command = shell.replace("<имя>", "<name>" if lang == "en" else "<名称>")
            content = (
                f"# Title\n\n{language_navigation(source, lang)}\n\n"
                f"<!-- i18n-source-git-blob: {digest} -->\n\n{target}\n\n"
                f"```bash\n{command}\n```\n"
            )
            translation(source, lang).write_text(content, encoding="utf-8")
        return source

    def test_existing_triad_and_russian_only_tool(self):
        self.write("tools/thing/README.md", "# Техническая документация\n")
        self.group("README.md", target="[Инструмент](tools/thing/README.md)")
        self.assertEqual(validate_repository(self.root), [])

    def test_all_languages_required_once_activated(self):
        self.group("README.md")
        (self.root / "README.zh-CN.md").unlink()
        self.assertTrue(any("README.zh-CN.md" in error for error in validate_repository(self.root)))

    def test_stale_source_marker(self):
        source = self.group("README.md")
        source.write_text(source.read_text(encoding="utf-8") + "\nИзменён контракт.\n", encoding="utf-8")
        self.assertTrue(any("не подтверждён" in error for error in validate_repository(self.root)))

    def test_invalid_language_navigation(self):
        self.group("README.md")
        english = self.root / "README.en.md"
        english.write_text(english.read_text(encoding="utf-8").replace("**English**", "English"), encoding="utf-8")
        self.assertTrue(any("переключатель" in error for error in validate_repository(self.root)))

    def test_missing_relative_documentation_target(self):
        self.group("README.md", target="[Ошибка](tools/not-existing/README.md)")
        self.assertTrue(any("неверная относительная ссылка" in error for error in validate_repository(self.root)))

    def test_keep_reader_language_for_localized_target(self):
        self.group("tools/thing/README.md")
        self.group("README.md", target="[Другой документ](tools/thing/README.md)")
        errors = validate_repository(self.root)
        self.assertTrue(any("другой язык" in e and "README.en.md" in e for e in errors))
        self.assertTrue(any("другой язык" in e and "README.zh-CN.md" in e for e in errors))

    def test_cross_document_links_are_valid_after_localization(self):
        self.group("tools/thing/README.md")
        self.group("README.md", target="[Другой документ](tools/thing/README.md)")
        for lang in ("en", "zh-CN"):
            path = self.root / f"README.{lang}.md"
            path.write_text(
                path.read_text(encoding="utf-8").replace(
                    "(tools/thing/README.md)", f"(tools/thing/README.{lang}.md)"
                ),
                encoding="utf-8",
            )
        self.assertEqual(validate_repository(self.root), [])

    def test_cli_commands_are_not_translated(self):
        self.group("README.md")
        english = self.root / "README.en.md"
        english.write_text(english.read_text(encoding="utf-8").replace("echo <name>", "other-tool <name>"), encoding="utf-8")
        self.assertTrue(any("исполняемые строки" in error for error in validate_repository(self.root)))

    def test_code_blocks_are_not_links(self):
        self.group("README.md")
        english = self.root / "README.en.md"
        english.write_text(english.read_text(encoding="utf-8").replace(
            "```bash\n", "```bash\n# [пример](missing-file.md)\n",
        ), encoding="utf-8")
        self.assertEqual(validate_repository(self.root), [])

    def test_code_block_types_must_be_preserved(self):
        self.group("README.md")
        english = self.root / "README.en.md"
        english.write_text(english.read_text(encoding="utf-8").replace("```bash", "```json"), encoding="utf-8")
        self.assertTrue(any("порядок или типы" in error for error in validate_repository(self.root)))

    def test_generic_future_docs_are_supported(self):
        self.group("README.md")
        self.group("docs/guide.md")
        self.assertEqual(validate_repository(self.root), [])

    def test_agents_and_skills_are_not_localization_targets(self):
        self.group("README.md")
        self.write("AGENTS.md", "# Внутренние правила\n")
        self.write(".codex/context/INDEX.md", "# Маршрутизация\n")
        self.write(".agents/skills/demo/SKILL.md", "# Skill\n")
        self.assertEqual(validate_repository(self.root), [])

    def test_orphan_translations_are_rejected(self):
        self.group("README.md")
        self.write("docs/orphan.en.md", "# Orphan\n")
        self.assertTrue(any("отсутствует русский оригинал" in error for error in validate_repository(self.root)))


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Проверка локализаций пользовательской документации (только стандартная библиотека)."""

import argparse
import hashlib
import re
import sys
from pathlib import Path
from urllib.parse import unquote, urlsplit

LANGUAGES = ("ru", "en", "zh-CN")
SOURCE_MARK = re.compile(r"<!-- i18n-source-git-blob: ([0-9a-f]{40}) -->")
LINK = re.compile(r"!?\[[^\]\n]*\]\((<?[^)\s>]+>?)(?:\s+\"[^\"]*\")?\)")
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")
PLACEHOLDER = re.compile(r"<[^<>\n]+>")


def translation(source: Path, language: str) -> Path:
    if language == "ru":
        return source
    return source.with_name(f"{source.stem}.{language}{source.suffix}")


def canonical(path: Path) -> Path:
    for language in LANGUAGES[1:]:
        suffix = f".{language}.md"
        if path.name.endswith(suffix):
            return path.with_name(path.name[: -len(suffix)] + ".md")
    return path


def managed_sources(root: Path) -> list[Path]:
    candidates = [root / "README.md", *root.glob("tools/" + chr(42) + "/README*.md"), *root.glob("docs/" + chr(42) + chr(42) + "/*.md")]
    return sorted({canonical(path) for path in candidates if path.is_file() or path == root / "README.md"})


def git_blob_sha(source: bytes) -> str:
    header = f"blob {len(source)}\0".encode("ascii")
    return hashlib.sha1(header + source).hexdigest()


def language_navigation(source: Path, language: str) -> str:
    names = {"ru": "Русский", "en": "English", "zh-CN": "简体中文"}
    chunks = []
    for lang in LANGUAGES:
        label = names[lang]
        chunks.append(f"**{label}**" if lang == language else f"[{label}]({translation(source, lang).name})")
    return " · ".join(chunks)


def markdown_parts(text: str) -> tuple[str, list[tuple[str, str]]]:
    """Исключить fenced-примеры из проверки ссылок; сохранить последовательность блоков."""
    visible = []
    blocks: list[tuple[str, str]] = []
    in_fence = False
    marker = ""
    min_length = 0
    kind = ""
    content: list[str] = []
    for line in text.splitlines():
        match = FENCE.match(line)
        if not in_fence and match:
            token = match.group(1)
            marker, min_length = token[0], len(token)
            kind = match.group(2).strip().split(" ", 1)[0].lower()
            in_fence = True
            content = []
            continue
        if in_fence:
            if match and match.group(1)[0] == marker and len(match.group(1)) >= min_length and not match.group(2).strip():
                blocks.append((kind, "\n".join(content)))
                in_fence = False
            else:
                content.append(line)
            continue
        visible.append(line)
    if in_fence:
        raise ValueError("незакрытый fenced code block")
    return "\n".join(visible), blocks


def bash_commands(body: str) -> list[str]:
    commands = []
    for line in body.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        # Переводимые имена каталогов-плейсхолдеров не являются CLI-контрактом.
        commands.append(PLACEHOLDER.sub("<ARG>", line))
    return commands


def links_are_valid(root: Path, page: Path, source: Path, language: str, visible: str) -> list[str]:
    errors = []
    for match in LINK.finditer(visible):
        address = match.group(1).strip("<>")
        parsed = urlsplit(address)
        if parsed.scheme or parsed.netloc or address.startswith(("#", "mailto:")):
            continue
        relative = unquote(parsed.path)
        if not relative:
            continue
        target = (root / relative.lstrip("/") if relative.startswith("/") else page.parent / relative).resolve()
        if not target.is_relative_to(root.resolve()) or not target.exists():
            errors.append(f"{page.relative_to(root)}: неверная относительная ссылка: {address}")
            continue
        # Сам переключатель языков намеренно ссылается на соседние версии.
        if page.parent == target.parent and target in {translation(source, lang).resolve() for lang in LANGUAGES}:
            continue
        target_source = canonical(target)
        if target_source.is_file() and target_source.suffix == ".md":
            expected = translation(target_source, language).resolve()
            if expected.is_file() and target != expected:
                errors.append(
                    f"{page.relative_to(root)}: ссылка {address} ведёт на другой язык; "
                    f"используйте {expected.relative_to(root)}"
                )
    return errors


def validate_group(root: Path, source: Path) -> list[str]:
    errors: list[str] = []
    siblings = {lang: translation(source, lang) for lang in LANGUAGES}
    if not source.is_file():
        return [f"{source.relative_to(root)}: отсутствует русский оригинал"]
    active = source == root / "README.md" or any(siblings[lang].is_file() for lang in LANGUAGES[1:])
    if not active:
        return []
    for lang, path in siblings.items():
        if not path.is_file():
            errors.append(f"{path.relative_to(root)}: отсутствует обязательная локализация {lang}")
    if errors:
        return errors

    contents = {lang: path.read_text(encoding="utf-8") for lang, path in siblings.items()}
    expected_sha = git_blob_sha(source.read_bytes())
    blocks = {}
    for lang, path in siblings.items():
        content = contents[lang]
        if language_navigation(source, lang) not in content.splitlines()[:25]:
            errors.append(f"{path.relative_to(root)}: отсутствует или неверно оформлен переключатель языков")
        if lang != "ru":
            markers = SOURCE_MARK.findall("\n".join(content.splitlines()[:25]))
            if markers != [expected_sha]:
                errors.append(
                    f"{path.relative_to(root)}: перевод не подтверждён для текущего русского оригинала "
                    f"(ожидается i18n-source-git-blob: {expected_sha})"
                )
        try:
            visible, blocks[lang] = markdown_parts(content)
        except ValueError as exc:
            errors.append(f"{path.relative_to(root)}: {exc}")
            continue
        errors.extend(links_are_valid(root, path, source, lang, visible))

    if len(blocks) != len(LANGUAGES):
        return errors
    original = blocks["ru"]
    for lang in LANGUAGES[1:]:
        translated = blocks[lang]
        if [kind for kind, _ in original] != [kind for kind, _ in translated]:
            errors.append(f"{siblings[lang].relative_to(root)}: порядок или типы блоков кода отличаются от оригинала")
            continue
        for index, ((kind, src), (_, dst)) in enumerate(zip(original, translated), 1):
            if kind in ("bash", "sh", "shell") and bash_commands(src) != bash_commands(dst):
                errors.append(
                    f"{siblings[lang].relative_to(root)}: исполняемые строки блока {index} "
                    "не совпадают с русским оригиналом"
                )
    return errors


def validate_repository(root: Path) -> list[str]:
    errors = []
    for source in managed_sources(root):
        try:
            errors.extend(validate_group(root, source))
        except (OSError, UnicodeError, ValueError) as exc:
            errors.append(f"{source.relative_to(root)}: ошибка проверки: {exc}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.cwd(), help="Корень репозитория")
    args = parser.parse_args()
    root = args.root.resolve()
    issues = validate_repository(root)
    if issues:
        for issue in issues:
            print(f"::error::{issue}", file=sys.stderr)
        return 1
    print("Локализации документации: источники, синхронизация, ссылки и команды проверены")
    return 0


if __name__ == "__main__":
    sys.exit(main())

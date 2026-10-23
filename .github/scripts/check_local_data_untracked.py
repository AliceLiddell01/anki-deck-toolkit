#!/usr/bin/env python3
"""Проверка, что пользовательские данные не попадают в индекс Git (только stdlib).

Пользовательские данные репозитория — локальные колоды и локальное доверенное
хранилище ресурсов — живут в игнорируемых Git областях:

* ``decks/``         — пользовательские данные целиком: ``decks/input/`` —
  закрытая область будущего доверенного загрузчика, ``decks/output/`` —
  плоский список рабочих локальных колод. Отслеживаемым внутри ``decks/``
  не может быть ни один путь, а не только эти две подобласти;
* ``.asset-store/``  — локальное доверенное хранилище ресурсов.

``.gitignore`` сам по себе не защищает: он не отменяет уже добавленный индекс и
обходится ``git add -f``. Поэтому проверка идёт по фактическому индексу, а
дополнительно подтверждается, что правила игнорирования на месте и не ослаблены
повторным включением.
"""

import argparse
import subprocess
import sys
import tempfile
from pathlib import Path

# Области, которые не могут быть отслеживаемыми ни при каких условиях.
FORBIDDEN_PREFIXES = (
    "decks/",
    ".asset-store/",
)
FORBIDDEN_ROOTS = ("decks", ".asset-store")

# Правила, которые обязаны присутствовать в корневом `.gitignore`.
REQUIRED_IGNORE_RULES = ("/decks/", "/.asset-store/")

IGNORE_PROBES = (
    "decks/.anki-repository-ignore-probe",
    "decks/output/.anki-repository-ignore-probe/deck.json",
    ".asset-store/.anki-repository-ignore-probe",
    ".asset-store/kanji/.anki-repository-ignore-probe",
)
TRACKED_CODE_PROBES = ("README.md", "tools/anki-repo/src/layout.rs")


def tracked_user_data(tracked_paths):
    """Возвращает отслеживаемые пути, лежащие в пользовательских областях."""
    violations = []
    for path in tracked_paths:
        normalized = path.replace("\\", "/")
        # Отбрасывается только буквальный префикс `./`: `.asset-store/` начинается
        # с точки, и вырезание множества символов уничтожило бы его имя.
        if normalized.startswith("./"):
            normalized = normalized[2:]
        if normalized in FORBIDDEN_ROOTS or normalized.startswith(FORBIDDEN_PREFIXES):
            violations.append(normalized)
    return violations


def ignore_rule_problems(gitignore_text):
    """Проверяет правила игнорирования по фактическому поведению Git."""
    problems = []
    lines = [line.strip() for line in gitignore_text.splitlines()]
    rules = {line for line in lines if line and not line.startswith("#")}

    for rule in REQUIRED_IGNORE_RULES:
        if rule not in rules:
            problems.append(f"в корневом .gitignore отсутствует правило {rule}")

    # Синтетический репозиторий проверяет именно семантику Git: разбор glob
    # вручную пропускает обобщённые исключения вроде `!/deck*/`.
    with tempfile.TemporaryDirectory(prefix="anki-ignore-guard-") as temporary:
        root = Path(temporary)
        (root / ".gitignore").write_text(gitignore_text, encoding="utf-8")
        initialized = subprocess.run(
            ["git", "init", "--quiet"], cwd=root, capture_output=True, check=False
        )
        if initialized.returncode != 0:
            detail = initialized.stderr.decode("utf-8", "replace").strip()
            raise RuntimeError(f"не удалось создать синтетический Git-репозиторий: {detail}")

        for path in IGNORE_PROBES:
            if not _git_ignores(root, path):
                problems.append(
                    f"правила .gitignore не игнорируют пользовательский путь {path}"
                )
        for path in TRACKED_CODE_PROBES:
            if _git_ignores(root, path):
                problems.append(f"правила .gitignore ошибочно игнорируют код {path}")

    return problems


def _git_ignores(root, path):
    """Проверяет путь без влияния индекса и глобальных exclude-настроек."""
    result = subprocess.run(
        [
            "git",
            "-c",
            "core.excludesFile=/dev/null",
            "check-ignore",
            "--no-index",
            "--quiet",
            "--",
            path,
        ],
        cwd=root,
        capture_output=True,
        check=False,
    )
    if result.returncode == 0:
        return True
    if result.returncode == 1:
        return False
    detail = result.stderr.decode("utf-8", "replace").strip()
    raise RuntimeError(
        f"git check-ignore не смог проверить {path}: {detail or result.returncode}"
    )


def tracked_paths(root):
    """Читает индекс Git; отсутствие git — ошибка проверки, а не пропуск."""
    result = subprocess.run(
        ["git", "ls-files", "-z"],
        cwd=root,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", "replace").strip()
        raise RuntimeError(f"не удалось прочитать индекс Git: {detail or result.returncode}")
    return [
        item.decode("utf-8", "surrogateescape")
        for item in result.stdout.split(b"\0")
        if item
    ]


def validate_tree(root):
    """Возвращает список нарушений границы пользовательских данных."""
    gitignore = root / ".gitignore"
    if not gitignore.is_file():
        return ["в корне репозитория нет .gitignore"]
    problems = ignore_rule_problems(gitignore.read_text(encoding="utf-8"))
    for path in tracked_user_data(tracked_paths(root)):
        problems.append(f"пользовательский путь отслеживается Git: {path}")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    args = parser.parse_args()
    try:
        errors = validate_tree(args.root)
    except RuntimeError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    if errors:
        for error in errors:
            print(f"::error::{error}", file=sys.stderr)
        return 1
    print("Пользовательские данные не отслеживаются Git, правила игнорирования на месте")
    return 0


if __name__ == "__main__":
    sys.exit(main())

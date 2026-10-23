#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(git -C "$script_dir" rev-parse --show-toplevel)"
tmp_repo="$(mktemp -d)"
trap 'rm -rf -- "$tmp_repo"' EXIT

cp -- "$repo_root/.gitignore" "$tmp_repo/.gitignore"
git -C "$tmp_repo" init --quiet

make_file() {
  local path="$1"
  mkdir -p -- "$tmp_repo/$(dirname -- "$path")"
  : > "$tmp_repo/$path"
}

assert_ignored() {
  local path="$1"
  local status=0
  if git -C "$tmp_repo" check-ignore --quiet -- "$path"; then
    return 0
  else
    status=$?
  fi

  if [[ "$status" -eq 1 ]]; then
    printf 'ОШИБКА: путь должен игнорироваться: %s\n' "$path" >&2
  else
    printf 'ОШИБКА: git check-ignore завершился с кодом %s для %s\n' "$status" "$path" >&2
  fi
  return 1
}

assert_included() {
  local path="$1"
  local status=0
  if git -C "$tmp_repo" check-ignore --quiet -- "$path"; then
    printf 'ОШИБКА: путь не должен игнорироваться: %s\n' "$path" >&2
    return 1
  else
    status=$?
  fi

  if [[ "$status" -ne 1 ]]; then
    printf 'ОШИБКА: git check-ignore завершился с кодом %s для %s\n' "$status" "$path" >&2
    return 1
  fi
}

# Пользовательские данные не публикуются через Git — ни целиком, ни частично.
# Whitelist-исключений внутри этих областей нет: игнорируется всё, включая
# owner-маркер, манифест, байты ресурсов, локальное состояние `.runtime` и
# приватный журнал операций. Имена экспортов здесь синтетические: проверка
# принадлежит `.gitignore`, а не составу рабочих колод пользователя.
ignored_paths=(
  '.asset-store/kanji/.owner.json'
  '.asset-store/kanji/manifest.json'
  '.asset-store/kanji/.lock'
  '.asset-store/kanji/assets/png/x.png'
  '.asset-store/kanji/assets/gif/x.gif'
  '.asset-store/kanji/assets/png/nested/deeper/file.png'
  '.asset-store/kanji/.tmp/transaction'
  '.asset-store/kanji/.runtime/candidates/manifest.json'
  '.asset-store/kanji/.runtime/batches/review-batch/review.html'
  '.asset-store/kanji/.journal/events.jsonl'
  '.asset-store/kanji/unknown.txt'
  '.asset-store/pitch-accent/.owner.json'
  '.asset-store/pitch-accent/manifest.json'
  '.asset-store/pitch-accent/assets/png/幽霊.png'
  '.asset-store/pitch-accent/.runtime/candidates/manifest.json'
  '.asset-store/pitch-accent/.runtime/batches/review-batch/review.html'
  '.asset-store/pitch-accent/.tmp/transaction'
  '.asset-store/pitch-accent/.lock'
  'decks/input/raw/anything.json'
  'decks/input/nested/deeper/file.bin'
  'decks/output/example-export/deck.json'
  'decks/output/example-export/media/漢.gif'
  'decks/output/example-export/nested/deeper/file.txt'
  # Прежняя раскладка с каталогами группировки — тоже пользовательские данные,
  # хотя и не `decks/input/` или `decks/output/`.
  'decks/legacy/words/example-export/deck.json'
)

directory_paths=(
  '.asset-store/'
  '.asset-store/kanji/'
  '.asset-store/kanji/assets/png/'
  '.asset-store/kanji/.journal/'
  'decks/input/'
  'decks/input/nested/'
  'decks/output/'
  'decks/output/example-export/'
  'decks/legacy/'
)

# Контрольные пути вне пользовательских областей. Без них проверка была бы
# ложноположительной: сломанный или пустой `.gitignore` «игнорировал» бы всё.
included_paths=(
  'README.md'
  'tools/asset-store/src/lib.rs'
)

for path in "${ignored_paths[@]}" "${included_paths[@]}"; do
  make_file "$path"
done

for path in "${directory_paths[@]}"; do
  mkdir -p -- "$tmp_repo/$path"
done

for path in "${ignored_paths[@]}"; do
  assert_ignored "$path"
done

for path in "${directory_paths[@]}"; do
  assert_ignored "$path"
done

for path in "${included_paths[@]}"; do
  assert_included "$path"
done

# Ничто из пользовательских областей не может попасть в индекс: `git add -A`
# уважает правила игнорирования, поэтому после него индекс обязан содержать
# только контрольные пути.
git -C "$tmp_repo" add -A
# Экранирование путей отключено: иначе не-ASCII имя (`漢.gif`, `幽霊.png`) выводится
# в кавычках с восьмеричными последовательностями и не совпадает с шаблоном утечки.
tracked="$(git -C "$tmp_repo" -c core.quotePath=false ls-files)"
leaked="$(printf '%s\n' "$tracked" | grep -E '^(decks/|\.asset-store/)' || true)"
if [[ -n "$leaked" ]]; then
  printf 'ОШИБКА: пользовательские данные попали в индекс:\n%s\n' "$leaked" >&2
  exit 1
fi

for path in "${included_paths[@]}"; do
  if ! printf '%s\n' "$tracked" | grep -qxF -- "$path"; then
    printf 'ОШИБКА: контрольный путь не попал в индекс: %s\n' "$path" >&2
    exit 1
  fi
done

# Проверка семантики `.gitignore` должна обнаружить повторное включение до того,
# как пользовательский путь попадёт в индекс.
python3 "$repo_root/.github/scripts/check_local_data_untracked.py" --root "$tmp_repo" >/dev/null
printf '\n!/deck*/\n' >> "$tmp_repo/.gitignore"
if python3 "$repo_root/.github/scripts/check_local_data_untracked.py" --root "$tmp_repo" >/dev/null 2>&1; then
  printf 'ОШИБКА: обобщённое повторное включение не остановлено до добавления в индекс\n' >&2
  exit 1
fi

printf 'Проверка .gitignore пройдена: %s игнорируемых путей и %s игнорируемых каталогов внутри `decks/` и `.asset-store/`; индекс содержит только %s контрольных путей.\n' \
  "${#ignored_paths[@]}" "${#directory_paths[@]}" "${#included_paths[@]}"

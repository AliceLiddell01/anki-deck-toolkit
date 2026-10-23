# Пакет pitch-accent и ручная проверка

Применяется, когда фактическая `.anki-repo/create.yaml` назначает `pitch_accent`
полю создаваемой заметки. Публичные команды и JSON принадлежат
`tools/asset-store/README.md`; здесь задана оркестрация. При расхождении проверь
`pitch-assets --help` и README владельца. Не заменяй отсутствующий API скриптом,
который меняет манифест или самостоятельно рисует/сравнивает графики.

## Зависимости и исходный снимок

Идентичность — точная `surface` слова; чтение `reading` уточняет поиск и не входит
в идентичность или имя файла. Получай оба значения из явно сохранённого
семантического входа новой заметки по сопоставлению в [SKILL.md](../SKILL.md).
Поле, назначенное `pitch_accent`, — потребитель результата, оно может быть
пустым при планировании и получении. Не извлекай слово из него. Пример
пустого выходного поля — [semantic-input.md](semantic-input.md).
Дедуплицируй одинаковые запросы; несовместимые чтения одной `surface` нельзя
молча объединять.

Хранилище — `.asset-store/pitch-accent` в корне checkout; `create` читает его
офлайн, и оно не публикуется через Git. Получение выполняет только `pitch-assets`.
Перед `batch start` проверь корпус через `pitch-corpus-gate` и сохрани полный
исходный JSON:

```bash
cargo run --locked --bin pitch-assets -- --output json corpus list
cargo run --locked --bin pitch-assets -- --output json corpus check
```

`corpus list` возвращает `operation = corpus_list`, `outcome = listed`,
`records[]` с записями `AssetRecord`, а не кандзи `assets[]`. Сохрани
`identity`, `sha256`, `storage_path`, `consumer_filename`, `lifecycle`,
`validation` и сведения домена для сравнения байтов и доверия. Для пригодной
записи `lifecycle = verified`, `validation.status = verified`,
`validation.content_sha256 = sha256`; актуальность валидатора и свидетельств
подтверждает `corpus check`/`pitch-corpus-gate`. Не вычисляй доверие только из
хеша или осмотра PNG, не разбирай манифест самостоятельно.

При отсутствии каталога **и** канонического корпуса пустой снимок допустим только
по точному отказу `corpus list`: код завершения `4`,
`outcome = failed`, `error.code = store_missing`. Условие кандзи
`outcome = blocked` сюда не переносится. Другая ошибка закрывает продолжение
работы. Снимок неизменен при возобновлении.

## Запуск и возобновление

Состоянием пакета владеет `pitch-assets` под
`.asset-store/pitch-accent/.runtime/batches/<batch-id>/`. ID и исходный план
сохраняй вне `decks/**` и Git. Тот же ID возобновляет тот же исходный план;
другой план отвергается (`identity_conflict`).

План готовится из семантического входа, без зависимости от выходного поля:

```json
{
  "schema_version": 1,
  "items": [
    { "surface": "幽霊", "reading": "ゆうれい" },
    { "surface": "飴" }
  ]
}
```

```bash
# Создать пакет; без --batch-id владелец выдаст ID.
cargo run --locked --bin pitch-assets -- --output json batch start \
  --batch-id <id> --plan <plan.json>

# Получить ресурсы текущего набора.
cargo run --locked --bin pitch-assets -- --output json batch run --batch-id <id>

# Продолжить прерванный прогон тем же путём.
cargo run --locked --bin pitch-assets -- --output json batch resume --batch-id <id>

# Перечитать состояние без сети и браузера, сверив его с владельцем корпуса.
cargo run --locked --bin pitch-assets -- --output json batch status --batch-id <id>
```

`batch status` может сохранить обновлённое состояние при сверке с корпусом;
его `changed` не обязательно `false`. Это офлайн путь перечитать пакет перед
действием. Ответ содержит `batch_id`, `batch`, `items`, `blockers`, `artifact`;
для элемента проверяй `surface`, `reading`, `status`,
`current_candidate_sha256`, `canonical_sha256` и `last_outcome`.
В pitch нет полей кандзи `counts.effective_verified`,
`items[].effective_verified` или `published_sha256`.

**Сессия браузера одна на хост.** Кандзи и pitch-accent делят каталог браузера.
Одновременные `batch run` могут завершить вторую сессию `browser_setup`
(`SingletonLock: File exists`). Запускай получение последовательно;
это технический отказ с возможностью повтора, а не отсутствие слова/ударения.

## Исходы и действия

Разрешённые статусы: `no_pitch_accent_on_source`, `published`,
`existing_verified`. Код завершения `0` от `batch status` означает успешное чтение,
а не разрешение каждого элемента; проверяй `items[].status` и `blockers`.

| Статус | Действие |
|---|---|
| `pending` | `batch run` или `batch resume` |
| `acquired_verified`, `publication_pending` | следующий `run`/`resume` завершит запись в локальный корпус |
| `existing_verified`, `published` | ресурс готов; сверь канонический SHA |
| `no_pitch_accent_on_source` | зависимость разрешена без медиа |
| `ambiguous_vocabulary` | явный `batch select`, затем `run` |
| `vocabulary_not_found` | зависимость не разрешена; уточни слово/чтение у пользователя |
| `technical_failure` | `batch retry`, только если отказ допускает повтор |
| `candidate_rejected` | `batch reacquire`, затем `run` |
| `conflict` | перечитай `batch status`, затем явный `batch reacquire` с `--reason` |

`no_pitch_accent_on_source` не создаёт пустую запись или изображение-заглушку;
поле остаётся пустым. `vocabulary_not_found` не доказывает отсутствие pitch:
не подставляй пустое поле и не заменяй запрошенное слово без основания.
Отказ `create` по занятому имени — отдельный конфликт прежнего имени.

При неоднозначности передавай объективно известное `reading`. Не выбирай
первого кандидата автоматически; если свежий поиск неоднозначен, нужен выбор
человека. Перед точечным решением создай отчёт:

```bash
cargo run --locked --bin pitch-assets -- --output json batch review --batch-id <id>

cargo run --locked --bin pitch-assets -- --output json batch select \
  --batch-id <id> --surface 重複語 --vocabulary-id <id> \
  --detail-url 'https://jpdb.io/vocabulary/<id>/重複語/ちょうふくご'
```

`artifact` — локальный HTML в игнорируемом служебном каталоге. Владелец сверяет ID и
`detail_url` со свежим списком кандидатов. `batch retry` при отказе без
возможности повтора возвращает `invalid_transition`; `batch reject` требует
точный текущий `--sha256` из истории кандидатов пакета.

## Доверие

Pitch принимает только текущее автоматическое `VERIFIED` ожидаемого валидатора
для точного SHA. Одобрение человека не заменяет это решение: нужен новый
`batch reacquire` с текущим валидатором. Доверие кандзи по решению человека
сюда не переносится. `create` не принимает `REJECTED`, `UNCERTAIN`, устаревшие
свидетельства или незавершённую запись в локальный корпус. При `pitch_asset_missing` или
`asset_integrity_invalid` сверь `batch status` и `corpus check`.

## Каноническое имя и прежний конфликт

Имя — `<surface>.pitch.png`. Прежнее `<surface>.png` может конфликтовать с
резервным изображением символа в общем `media/`; `create` отказывает
(`destination_media_conflict`). Разрешай конфликт через `anki-repo migrate-media`,
которая доказывает связь идентичности с прежним именем и семантику каждого живого
потребителя. Порядок — [SKILL.md](../SKILL.md), раздел «Прежнее имя
файла-потребителя»; контракт — `tools/anki-repo/README.md`.
Не переименовывай файл и не правь `deck.json` руками, не заканчивай набор на
N−1 заметках и не меняй слово из-за занятого имени.

## Локальное состояние корпуса

Корпус pitch-accent — локальное доверенное состояние в
`.asset-store/pitch-accent`: он не публикуется через Git, отдельных ветвей
публикации ресурсов и проверки прав администратора для ресурсов не существует.
До продвижения заметок повтори **его** `corpus list`, `corpus check` и
`batch status`. Все `items[].status` должны быть разрешены, `blockers` пусты. Для
`published`/`existing_verified` запись `records[]` этой `surface` имеет
`sha256 = items[].canonical_sha256` и текущее автоматическое доверие;
у записанного в локальный корпус кандидата сверь также
`current_candidate_sha256`. `no_pitch_accent_on_source` разрешает элемент без
записи и файла; исход провайдера в `last_outcome` и `batch` служит свидетельством
отсутствия pitch, а не пустой снимок владельца.

Сравни записи по идентичности/SHA/доверию с сохранённым исходным снимком. Разница
должна принадлежать исходному плану пакета, а независимые идентичности
сохраняются. Посторонняя разница означает, что хранилище изменилось помимо пакета,
— сохрани состояние и разберись, не запуская новое получение.

Проверку корпуса запускай текущим кодом из корня checkout:

```bash
cargo run --quiet --locked -p asset-store --bin pitch-corpus-gate
```

После возобновления повтори **pitch** снимок владельца и проверку корпуса; сверь
сохранность независимых идентичностей и SHA/доверие пакета.

## Продвижение и проверка карточек

После каждого раунда перечитай состояние и продвигай заметки, чьи **все**
зависимости разрешены; независимые не ждут остальных. Для доказанного отсутствия
pitch поле остаётся пустым, ожидающие ресурсы не получают заглушек.
До первого `--apply` сними состояние «до» вне `decks/**`. После записи —
`anki-repo validate` и `visual-report` с `--before`/`--after`: PNG читается
на тёмном фоне, назначенное поле пусто либо ссылается на
`<surface>.pitch.png`, все медиассылки работают и прежнее имя не подменяет
каноническое. Отчёт держи вне `decks/**` и Git. Повтор сходится к no-op.

Контракт команд —
[`tools/asset-store/README.md`](../../../../tools/asset-store/README.md) и
[`tools/anki-repo/README.md`](../../../../tools/anki-repo/README.md).

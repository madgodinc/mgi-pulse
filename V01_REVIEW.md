# mgi-pulse v0.1 — снимок для жёсткой критики (раунд 2)

Дата: 2026-06-01, после round 1 от критика. **Этот файл — для прогона
через критика. Не часть проектной документации.** Включён в репо, чтобы
у критика был стабильный URL и видна история работы; после релиза будет
удалён.

Репо: https://github.com/madgodinc/mgi-pulse  
Главная ветка: main, **не push'нут v0.1.0 тэг** — ещё работаем.

---

## Что изменилось с round 1

Критик дал 8 пунктов: Q2 (FieldAccess), Q3 (owned_lines память), Q6
(less-mode majority), Q7 (histogram cache), Q8 (mouse без opt-out), Q10
(`--time-field`/`--level-field`), Q11 (perf), Q12 (integration-тесты),
+ SIGBUS-доку, + Q13 длинные строки.

Из них **закрыты в M3.8 (`b31950f`) и M3.9 (`712e562`):**

| Что | Где |
|---|---|
| ✅ Histogram cache key = (generation, bars) | M3.8 |
| ✅ owned_lines sparse HashMap (–176 МБ на 11M) | M3.8 |
| ✅ Less-mode majority (>50%) | M3.8 |
| ✅ Real `--no-mouse` флаг | M3.8 |
| ✅ DetailPane long-line cap (256 КБ) | M3.8 |
| ✅ SIGBUS docs (README + source + --help) | M3.9 |
| ✅ `--time-field` / `--level-field` / `--columns` | M3.9 |
| ✅ `R` rescan-schema клавиша | M3.9 |
| ✅ Pipeline integration тесты (6 кейсов) | M3.9 |

**Сознательно отложено** (с обоснованием):

- **Полный stream warmup-timer (5s + has_seen_fields):** требует переписать
  indexer на batch-flush протокол с mid-drain schema lock. Это вторжение
  в hot path. `R` клавиша покрывает практический кейс «boot-banner залочил
  не ту схему». Тимер сделаем в v0.2 вместе с native follow.
- **TS_UNTIMED как флаг-бит вместо `i64::MIN`:** теоретическая коллизия с
  легитимным epoch около 1970. В реальных логах не встречается.
- **Q1 «parse-once между producer/schema/predicate»:** записан как явный
  долг в комментариях. Не оптимизируется пока не упирается в профайл.
- **Q2 FieldAccess trait** для предикатов — заложен как **шаг 1 плана
  v0.1** (format dispatch + FieldAccess делается одновременно). Сейчас
  predicate-машина под NDJSON, как было; критик правильно указал что без
  FieldAccess формат-абстракция не построится. Делается перед logfmt/EDN.

---

## Где сейчас v0.1

**В коде (текущий main, коммит `712e562`):**

- 11 коммитов: M0 → M3.9.
- ~4 100 строк Rust (core ~2 500, tui ~1 350, bench ~210, tests ~120).
- **48 тестов:** 42 unit (mgi-pulse-core) + 6 integration (pulse-tui/tests/cli.rs).
- `cargo build --release` 6-7s, бинарь **3.0 МБ**.
- **2 ГБ NDJSON → 2.7 s end-to-end indexing** (i5-12400F).
- Memory: ~280 МБ индексов на 11M записей (после sparse-fix).

**CI:** `.github/workflows/ci.yml` (3-OS matrix × fmt/clippy/test).
Release.yml на тэге `v*` → musl-static x86_64 linux + draft Release.

**README** содержит явно:
- Why / Install / Quickstart с less-mode секцией.
- **Static files vs live files (mmap safety)** — про SIGBUS.
- **Mouse capture and terminal selection** — про Shift+drag, `--no-mouse`.
- Keyboard reference (всё включая `t`, `R`, `0-4`, `m`, `d`, `Ctrl-T/W`).
- What it doesn't do (yet) — честный список плюс EDN-планы.
- License: Apache-2.0.

**Что ещё в плане до push'а v0.1.0:**

После закрытия 8 bugfix'ов (это раунд критика) — большой скоуп:

1. Format dispatch trait + FieldAccess (Q2 архитектура + база)
2. logfmt parser (Go/Heroku)
3. .gz / .zst input wrapper
4. EDN parser (Clojure)
5. Multi-line detection
6. Python logging parser
7. SQL virtual table через rusqlite (embedded, не БД)
8. Native `tail -F` (inotify/kqueue)
9. Темы (dark/light/nocolor)
10. Bookmarks (`b` / `B`)

Грубая оценка: ~10-12 дней part-time, ~2-3 недели календарно.

---

## Архитектурные узлы по которым хочу критики

### 1. Format dispatch — правильно ли я думаю про FieldAccess?

Идея (после критики Q2): ввести

```rust
trait FieldAccess {
    fn get(&self, key: &str) -> Option<&str>;
    fn ts(&self) -> Option<i64>;
    fn level(&self) -> u8;
}

trait LogFormat {
    fn detect(sample: &[u8]) -> bool;
    fn parse<'a>(&self, line: &'a [u8]) -> Box<dyn FieldAccess + 'a>;
}
```

Producer'ы держат `Box<dyn LogFormat>`. Predicate работает над
`&dyn FieldAccess`, не над сырыми байтами.

Что меня беспокоит:
- **Аллокация на запись.** `Box<dyn FieldAccess>` per line × 11M = ~176 МБ
  vtable+box-аллокаций. Можно вернуть `impl FieldAccess` через generic, но
  тогда не работает динамический dispatch разных форматов в одном
  MergeProducer'е.
- **Lifetime borrow** через trait object — `'a` пробрасывается через
  `dyn`, это в Rust работает но синтаксически громоздко.
- **Альтернатива:** enum `ParsedFields { Ndjson(NdjsonFields), Logfmt(LogfmtFields), … }`
  — закрытое множество форматов, нет vtable, есть `match`. Минус — не
  расширяется юзером плагинами. Но v0.1 расширений не предусматривает.

Вопрос: enum или trait object? Что менее жалкое архитектурно?

### 2. SQL через rusqlite virtual table — реально или утопия?

План:
- `rusqlite` с feature `vtab`.
- Кастомный virtual table `logs(line_id, ts, level, source_id, raw)` +
  `JSON_EXTRACT(raw, '$.field')` для динамических полей.
- `xBestIndex` callback использует наши `TimeIndex`/`SeverityIndex`
  индексы — то есть SQLite запрос `WHERE level='error' AND ts > X`
  попадает в наш bitmap scan, **не** full scan по 11M строкам.
- TUI prompt `:` для SQL-режима. Результат рендерится в той же таблице
  с колонками из `SELECT`.

Что меня беспокоит:
- **Оверхед vtable callback'ов.** Каждое обращение к колонке = call
  через FFI в наш Rust код. На 11M строк × несколько колонок это десятки
  миллионов callback'ов. Профайл выглядит страшно.
- **JSON_EXTRACT lazy parse** требует парсить JSON строки повторно (раз
  за раз) — это против моего же принципа parse-once. Без кэширования
  полей запросы с 3+ полями в `WHERE` будут парсить каждую строку
  3+ раз.
- **xBestIndex** API нетривиальный — нужно сообщить SQLite cost estimates
  и сделать handshake что мы реально умеем ускорить запрос. Если ошибся —
  SQLite сделает full scan, ничего не сказав.

Вопрос: реально ли это **закроется в 4-6 дней** или это месяц минимум?
Если месяц — может стоит начать с **простого "SQL-like prompt"** (наш
own DSL который компилируется в Predicate-AND-композицию)?

### 3. Multi-line records — где правильно делать склейку?

Для Python tracebacks / Java stacks нужно склеивать continuation lines
(`^\s+`) в parent record. Где:

- **(a)** В Producer'е — он скрывает continuation от Indexer'а. `next()`
  возвращает `RawRecord` где `bytes` = `Box<[u8]>` со склеенным
  multi-line содержимым.
- **(b)** В Indexer'е — Producer выдаёт каждую строку отдельно, Indexer
  смотрит формат, решает «это continuation предыдущего line_id» и
  обновляет parent.
- **(c)** Отдельный `MultiLineProducer<P>` wrapper над любым Producer'ом.

Я склоняюсь к **(c)** — wrapper. Producer (NDJSON/logfmt/python) ничего
не знает про continuation; MultiLineProducer спрашивает у формата «это
continuation?» (вынести в `LogFormat::is_continuation`) и склеивает.

Минус: bytes хранятся в `Owned(Box<[u8]>)` всегда (нельзя сохранить
FileRef если нужно склеить несколько диапазонов). Для файлов с большим
% multi-line это съест mmap-выигрыш по памяти.

Вопрос: (a)/(b)/(c) — какой меньше всего плох?

### 4. CHANGELOG + version bumps — стратегия?

Сейчас CHANGELOG имеет только `[0.1.0]` секцию. Все мои bugfix'ы
M3.5-M3.9 написаны как «постепенный путь к v0.1.0», ещё не release'ы.

Когда release'нем v0.1.0 и добавим logfmt — это v0.2.0 или v0.1.x?
Semver говорит: новый формат — это feature, minor bump. То есть v0.2.
Но **до релиза 0.1.0** мы продолжаем добавлять, и CHANGELOG показывает
тот же `0.1.0` с растущим списком фич. Это honest?

Альтернатива: остановить scope-расширение, релизнуть v0.1.0 как есть
(NDJSON-only + less-mode + 48 тестов + bugfix'ы), потом форматы как
v0.2.0. Но Mad решил иначе — все 10 фич в v0.1.

Вопрос: уважать semver и переименовать в v0.2.0/v0.3.0 при разрастании,
или v0.1.0 с большим scope корректно?

---

## Текущий код — на что стоит посмотреть

| Файл | За что отвечает |
|---|---|
| `mgi-pulse-core/src/engine/mod.rs` | `Engine`, `has_*` probes, `scan_schema`, `rescan_schema` |
| `mgi-pulse-core/src/engine/parse.rs` | RFC3339 parser, `ts_and_level`, `ts_and_level_named`, `FieldNames` |
| `mgi-pulse-core/src/engine/indexer.rs` | `drain()` — main pipeline |
| `mgi-pulse-core/src/engine/predicate.rs` | `Predicate` trait + 4 impls + `AndPredicate` |
| `mgi-pulse-core/src/io/file.rs` | mmap FileProducer + SIGBUS docs |
| `mgi-pulse-core/src/io/stream.rs` | stdin StreamProducer |
| `mgi-pulse-core/src/io/merge.rs` | k-way merge |
| `pulse-tui/src/app/mod.rs` | 800-строчное сердце TUI: View, App, run-loop, mouse, tabs |
| `pulse-tui/src/panes/{table,timeline,detail}.rs` | Рендереры |
| `pulse-tui/tests/cli.rs` | Integration тесты |
| `bench/parse-bench/BENCH.md` | Парс-бенч цифры |

---

## Финальный вопрос к критику

В round 1 ты сказал **«не лезь в большие фичи, релизь то что есть»**.
Mad решил иначе — будет 10 фич в v0.1. Это твоя главная претензия от
round 1, я её **не закрыл** (нельзя — это решение пользователя).

Дай **критику с учётом** что v0.1 будет с большим scope. Что я должен
сделать **до начала шага 1** (FieldAccess архитектура) чтобы не пожалеть
позже? Какие из 8 bugfix'ов я **закрыл криво** и нужно переделать?

Не давай советы «релизни как есть» — это решение уже принято. Дай
советы как **минимизировать ущерб от расширенного scope**.

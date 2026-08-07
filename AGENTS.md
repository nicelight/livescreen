# AGENTS.md — правила для coding-агентов LiveScreen

Этот файл — рабочая инструкция для агентов, изменяющих репозиторий.

## Назначение проекта

LiveScreen — Linux/Ubuntu-приложение на Rust для записи экрана со звуком в MP4. Камера захватывается отдельным V4L2/GStreamer-потоком и показывается круглым desktop overlay поверх рабочего стола. В recording pipeline камера не добавляется: на Wayland экран захватывается через xdg-desktop-portal/PipeWire, на X11 используется существующий backend.

Проект придерживается архитектуры модульного монолита с вертикальными срезами. Конфигурация, устройства, preview, desktop overlay, recorder, portal и hotkeys физически выделены в модули `src/`; `src/main.rs` остаётся composition root для запуска процесса, GTK application shell, lifecycle и связывания этих срезов. Срез владеет своей ответственностью и ресурсами, а взаимодействие между потоками идёт через доменные типы и каналы. Новое физическое выделение оправдано только если оно улучшает тестируемость, владение ресурсом или изоляцию, а не просто уменьшает размер файла.

## Обязательная точка входа и завершение работ

Перед тем как приступать к любым работам в репозитории — включая анализ, диагностику, планирование, изменение кода, документации, конфигурации или упаковки — обязательно прочитай:

1. [`index-codebase.md`](index-codebase.md) — индекс кодовой базы, архитектуры, потоков, runtime-файлов и связей между файлами.
2. [`README.md`](README.md) — команды запуска, конфигурация и ручная проверка.
3. `Cargo.toml` и только те участки `src/main.rs`, `data/` или `scripts/`, которых касается задача.

Не восстанавливай архитектуру по памяти и не начинай с широкого переписывания. Сначала найди существующую точку интеграции и сохрани текущие границы потоков.

По окончании любых работ обязательно проверь и обнови [`index-codebase.md`](index-codebase.md), чтобы он отражал актуальные структуру проекта, архитектуру, потоки, runtime-файлы и связи между файлами.

## Reasoning Policy: Selection Before Expansion (KISS Gate)

**Core rule:** A sufficient solution is a reason to stop expanding, not an
invitation to add optional improvements.

Within the discretion left to you, select the simplest solution sufficient for
the requested outcome and required contracts. Treat every added mechanism,
abstraction, safeguard, edge-case handling, future-proofing measure, or process
as a separate candidate. Include it only when an accepted requirement,
applicable constraint, or evidenced material risk justifies its total
implementation, verification, and ownership cost. Possible usefulness, best
practice, or greater completeness is not sufficient justification.

A discovered risk, edge case, or possible failure is not automatically a
requirement. Assess its realism, impact, recoverability, and remedy cost in the
current deployment. Use the cheapest sufficient remedy for an accepted
requirement. If an uncovered serious problem requires expanding the accepted
target, ask the operator. Otherwise accept or defer a risk whose remedy costs
materially more than its expected impact. Agent discretion permits only a local
safeguard with negligible cost and no new state or lifecycle.

If an additional mechanism compensates for a weakness introduced by the base
solution, first revise or simplify the base candidate. Stop when the outcome and
required contracts are satisfied.

Accepted requirements authorize outcomes, not unnecessarily complex
mechanisms. Agent-generated reviews, specifications, brainstorm results, and
best-practice recommendations cannot authorize their own complexity. Do not
reopen accepted requirements, operator decisions, or governing sources.

Report evidenced defects and issues affecting the requested verdict. Do not
propose or report optional improvements, rejected candidates, or speculative
observations unless the user explicitly asks for them.


## Рабочий процесс агента

Агент может сам запускать debug/release-бинарь в текущем Wayland-сеансе, собирать stdout/stderr и application/GStreamer-логи, проверять процессы и MP4 через `ffprobe`. Физический ввод в системные диалоги и на клавиатуре по-прежнему выполняет оператор.

1. Сформулируй кратко причину изменения и затронутые файлы.
2. Проверь текущий код, конфигурацию и состояние сборки.
3. Внеси минимальное связное изменение в исходные файлы.
4. Добавь или обнови тесты для изменённого поведения.
5. Запусти форматирование, компиляцию, тесты и Clippy.
6. Проверь runtime-интеграции, если менялись GTK, Wayland/portal, GStreamer, устройства, hotkeys или упаковка.
7. Обнови документацию и обязательно проверь и синхронизируй `index-codebase.md` с итоговым состоянием репозитория.
8. В handoff укажи изменённые файлы, проверки и известные ограничения.

## Идиоматичный Rust

- Используй Rust 2024 и существующий стиль проекта.
- Владение, заимствования и типы должны выражать время жизни и ответственность за ресурс. Не добавляй `clone()` только для обхода borrow checker: объясни необходимость или перестрой границу владения.
- Для recoverable ошибок возвращай `Result`/`Option`; используй `?` и добавляй контекст через `anyhow::Context` или осмысленный тип ошибки.
- Не используй `unwrap()`, `expect()` и `panic!()` на внешнем вводе, в UI callbacks, при работе с файлами, устройствами, portal или GStreamer. Допустимы только доказанные инварианты; рядом оставь короткий комментарий, почему panic безопасен.
- Не скрывай ошибки через `let _ = ...`, если ошибка влияет на запись, сохранение, разрешения, закрытие pipeline или состояние UI. Либо обработай её, либо добавь понятный fallback/лог.
- Предпочитай маленькие функции с одной ответственностью и типы, которые не допускают неверное состояние. Для разных смыслов не передавай голые `bool`, числа или строки без необходимости — используй enum/newtype.
- Реализуй стандартные derive (`Debug`, `Clone`, `PartialEq`, `Eq`, `Default`) только когда они действительно соответствуют семантике типа.
- `unsafe` не добавляй без крайней необходимости. Если он нужен, изолируй небольшой участок и документируй инварианты безопасности.
- Не добавляй макросы, глобальное состояние или новую abstraction layer ради нескольких строк.

## Ошибки и ресурсы

- Ошибки на границах ОС должны сообщать действие и объект: что не удалось сделать, с каким устройством/путём/операцией и что может сделать пользователь.
- Закрывай GStreamer pipeline через корректный EOS, когда формат/контейнер требует финализации.
- Для потоков, каналов, файлов, FD, pipeline и GTK-объектов заранее определи владельца и порядок остановки.
- GTK объекты и обновление виджетов выполняются только в GTK main thread. Worker-потоки передают данные через каналы или другой явно безопасный механизм.
- Никогда не блокируй GTK main thread ожиданием portal, GStreamer, камеры, аудио или `JoinHandle`. Долгие операции выполняй в worker thread и возвращай события в UI.
- Любая остановка должна быть идемпотентной и не оставлять dangling thread, открытый FD или незавершённый MP4.

## Проектные границы LiveScreen

- Логические вертикальные срезы должны сохранять чёткие границы ответственности внутри модульного монолита.
- `src/main.rs` содержит только bootstrap процесса, GTK application shell/lifecycle и связывание существующих срезов через callbacks, timers и каналы.
- `main` связывает GTK UI и lifecycle; `PreviewController` отвечает за camera preview; `run_recorder` — за запись и EOS; `start_global_shortcuts` — за portal hotkeys. Не смешивай эти ответственности.
- Для Wayland используй xdg-desktop-portal/PipeWire, для X11 — существующий backend. Не обходи пользовательское разрешение экрана скрытым захватом.
- Внешние устройства и источники считаются отсутствующими или нестабильными: проверяй список, показывай понятный статус и не падай из-за `/dev/video*`, PulseAudio source или portal.
- Настройки JSON должны быть обратно совместимыми: добавляй `serde(default)` для новых полей, сохраняй валидные старые конфиги и проверяй диапазоны значений.
- Глобальные hotkeys должны отправлять доменные команды, а не напрямую менять GTK widgets из фонового потока.
- Изменения GStreamer pipeline требуют проверки всех используемых element factory и обновления списка bundled plugins в `scripts/build-appimage.sh`.

### Политика роста `src/main.rs`

- Новую slice-specific логику добавляй в соответствующий существующий модуль (`configuration`, `devices`, `preview`, `overlay`, `recorder`, `portal`, `shortcuts`), а в `main.rs` оставляй только создание компонента и его подключение к GTK lifecycle.
- Новый модуль создавай, когда появляется связная ответственность, которая владеет отдельным ресурсом или lifecycle, содержит независимо тестируемую доменную логику либо требует изоляции от GTK main thread. Само увеличение числа строк таким основанием не является.
- Одноразовое создание GTK-виджетов, небольшой callback и прямое связывание уже существующих компонентов оставляй в `main.rs`, если вынос не улучшает тестируемость или владение.
- Не создавай пустые модули «на будущее», общий `helpers.rs`, контейнер `AppState` со всем приложением или публичный API только ради декомпозиции.
- Если вынос требует длинного списка несвязанных аргументов, дополнительных `clone()` либо структуры, собирающей большинство GTK-виджетов и runtime-ресурсов, граница выбрана неверно: оставь код на месте или найди меньшую ответственность.

## Зависимости и Cargo

- Добавляй зависимость только при явной необходимости. Перед добавлением проверь, нельзя ли решить задачу стандартной библиотекой или уже установленным crate.
- Обновляй `Cargo.lock` вместе с `Cargo.toml` и проверяй, что выбранные features минимальны.
- Не меняй edition, версии GTK/GStreamer или профиль release без причины, связанной с задачей.
- Не редактируй `Cargo.lock` вручную.
- Не коммить временные логи, пользовательские конфиги, записи экрана, `target/` и случайные скачанные артефакты.

## Тестирование и quality gate

Перед handoff запусти из корня проекта:

```bash
cargo fmt -- --check
cargo check
cargo test
cargo clippy --all-targets -- -D warnings
```

Если менялась упаковка или GStreamer:

```bash
./scripts/build-appimage.sh
./LiveScreen-x86_64.AppImage --diagnose
```

Тесты должны проверять поведение, а не детали реализации. Для async/thread/UI-кода отделяй чистую логику в тестируемые функции, а внешние portal, устройства и display проверяй отдельным smoke test.

Для записи проверяй не только exit code: MP4 должен корректно завершаться, иметь ожидаемую длительность и содержать нужные codec streams через `ffprobe`.

## Форматирование, Clippy и комментарии

- Источник форматируется `rustfmt`; не исправляй форматирование вручную вопреки ему.
- Исправляй предупреждения Clippy по существу. `#[allow(...)]` используй локально, с причиной; не отключай широкие группы lint без объяснения.
- Комментарии объясняют «почему», ограничения ОС/portal/GStreamer и инварианты владения. Не комментируй очевидный синтаксис.
- Публичные API и нетривиальные типы документируй rustdoc-комментариями; для ошибок, panic и safety указывай ожидаемое поведение.
- Пользовательские сообщения и README в этом проекте пишутся на русском; имена Rust-символов и технические идентификаторы — на английском.

## Исходные файлы против артефактов

Редактируй исходные файлы:

- `src/main.rs`
- `Cargo.toml`
- `data/`
- `scripts/`
- `README.md`, `index-codebase.md`

Не редактируй вручную как источник истины:

- `AppDir/` — staging tree, пересоздаётся сборочным скриптом;
- `LiveScreen-x86_64.AppImage` — финальный бинарный артефакт;
- `tools/*.AppImage` — внешние инструменты упаковки;
- `target/` — Cargo build output.

Для изменения AppImage исправь `data/` или `scripts/build-appimage.sh` и пересобери пакет.

## Безопасность изменений

- Не удаляй и не перезаписывай широкие каталоги без точного подтверждения цели.
- Не трогай пользовательские `~/.config/livescreen/*` без явной необходимости; при миграции делай резервно-совместимое чтение.
- Не добавляй сбор telemetry, сетевые вызовы, экспорт видео или доступ к новым устройствам без отдельного требования.
- При изменении разрешений Wayland, desktop entry или global shortcuts проверь app id, регистрацию host application и отсутствие регрессии запуска.

## Официальные ориентиры

При спорных решениях сверяйся с первичными источниками:

- [The Rust Programming Language](https://doc.rust-lang.org/book/) — ownership, borrowing, error handling, concurrency и testing.
- [Cargo Book](https://doc.rust-lang.org/cargo/) — manifests, dependencies, profiles, lockfile и builds.
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) — naming, type safety, errors, documentation и interoperable APIs.
- [Clippy usage](https://doc.rust-lang.org/stable/clippy/usage.html) — lint policy и запуск `cargo clippy`.

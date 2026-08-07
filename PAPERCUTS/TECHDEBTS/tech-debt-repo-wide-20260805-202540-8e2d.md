# LiveScreen — repo-wide technical-debt report

Дата аудита: 2026-08-05

## Область и критерий включения

Проверены `src/`, `Cargo.toml`, `Cargo.lock`, `data/`, `scripts/`, `README.md`,
`index-codebase.md`, `FINDINGS_stack.md` и доступные runtime/staging-артефакты.
Включены только долги, для которых текущий код даёт точный путь исполнения и
которые могут заметно повлиять на пользовательский результат, shutdown,
поставку или проверяемость. Уже исправленные пункты из
`FINDINGS_stack.md` не дублируются.

Quality gates, выполненные без изменения исходников:

- `cargo fmt -- --check` — успешно;
- `cargo check` — успешно;
- `cargo test` — успешно, 23 теста;
- `cargo clippy --all-targets -- -D warnings` — успешно.

AppImage build и запуск `--diagnose` намеренно не выполнялись: build-скрипт
удаляет и пересоздаёт `AppDir`, а запуск приложения до разбора флага записывает
пользовательский desktop entry. Это не позволяет выполнить их в рамках
аудита с условием «изменить только этот отчёт».

## Сводка

Подтверждённых findings: 7.

| ID | Priority | Подтверждённый долг | Основной риск |
|---|---|---|---|
| TD-002 | P1 | Список камер шире возможностей preview, ошибки preview теряются | Камера отображается как доступная, но overlay остаётся пустым без причины |
| TD-003 | P2 | Перезапуск preview не сериализован | Старый и новый V4L2 pipeline могут работать одновременно |
| TD-004 | P2 | Global-hotkeys worker не имеет владельца lifecycle и retry-сигнала | Поток/portal-сессия не управляются при закрытии, сбой не восстанавливается без перезапуска |
| TD-005 | P2 | AppImage сборка зависит от `continuous` downloads и host GStreamer | Один и тот же исходный код даёт разные или неполные release-артефакты |
| TD-006 | P2 | `--diagnose` имеет побочный write-path | Диагностика меняет пользовательское состояние и может не стартовать из-за прав записи |
| TD-007 | P2 | Нет автоматической проверки реального portal-to-MP4 runtime | Unit-тесты не обнаруживают поломки PipeWire, устройств, bundled plugins и codec streams |
| TD-008 | P3 | Application shell остаётся высокосвязанным в `main.rs` | Изменения lifecycle/UI трудно проверять изолированно |

## Findings

### TD-002 — camera discovery обещает больше, чем поддерживает preview; ошибки скрыты

**Priority:** P1

**Exact scope:** обнаружение V4L2-устройств и запуск camera preview;
`src/devices.rs:28-53`, `src/preview.rs:29-70`, `src/main.rs:361-364`.

**Evidence:**

- `src/devices.rs:28-42` добавляет `/dev/video*`, если `v4l2-ctl` вывел
  строку `Size`; проверка не требует JPEG-capability.
- `src/preview.rs:35` строит единственный pipeline с жёстким
  `image/jpeg,width=640,height=360,framerate=30/1` и `jpegdec`.
- `src/preview.rs:38-52` без события или сообщения возвращает из worker при
  ошибке parse/downcast, отсутствии sink или `set_state`.
- `src/main.rs:361-364` запускает preview через метод с `()`-результатом; в
  этой границе нет канала ошибки для status label.

**Impact:** камера с raw/YUYV-режимом может попасть в список, но не пройти
  единственный JPEG pipeline. При любой такой ошибке пользователь видит
  пустой/необновляемый overlay и не получает actionable причины; при этом
  приложение продолжает выглядеть рабочим.

**Smallest remediation direction:** ввести один typed preview result/event для
  `Started`/`Error` и показать ошибку в GTK. Одновременно либо фильтровать
  список по фактически поддерживаемому JPEG-режиму, либо заменить жёсткие caps
  на согласование с capability устройства. Достаточно выбрать один из двух
  вариантов согласования, но discovery и pipeline должны иметь общий контракт.

### TD-003 — смена камеры запускает новый preview до завершения старого

**Priority:** P2

**Exact scope:** `PreviewController::start/stop` и callback смены камеры;
`src/preview.rs:24-104`, `src/main.rs:365-373`.

**Evidence:**

- `src/preview.rs:24-27` вызывает `self.stop()`, а затем сразу создаёт новый
  worker pipeline.
- `src/preview.rs:76-104` не ждёт старый worker: `JoinHandle` переносится в
  отдельный reaper thread, поэтому `stop()` возвращается до перехода старого
  pipeline в `Null`.
- `src/main.rs:368-373` вызывает `start()` непосредственно при изменении
  combo-box, то есть новый запуск не ждёт завершения предыдущего.
- `FINDINGS_stack.md:51-61` содержит независимое описание этого
  воспроизводимого lifecycle-разрыва.

**Impact:** короткое время могут одновременно обращаться к одному
  `/dev/video*` старый и новый pipeline; возможны отказ открытия устройства,
  гонки остановки и появление устаревших кадров, поскольку оба worker-а
  отправляют кадры в общий channel.

**Smallest remediation direction:** оставить GTK non-blocking, но сериализовать
  stop/start внутри одного владельца preview worker-а. Минимально достаточный
  контракт — запускать новый pipeline только после подтверждения завершения
  старого и отбрасывать кадры старого поколения.

### TD-004 — global hotkeys не имеют управляемого lifecycle и не восстанавливаются

**Priority:** P2

**Exact scope:** запуск portal Global Shortcuts и application shutdown;
`src/shortcuts.rs:20-80`, `src/main.rs:193-244`, `src/main.rs:656-678`.

**Evidence:**

- `src/shortcuts.rs:25-78` создаёт named thread, но возвращает только
  `Result<()>`; `JoinHandle`, cancellation channel и handle portal session
  владельцу не передаются.
- Ошибка создания `WindowIdentifier` в `src/main.rs:222-231` или ошибка
  `start_global_shortcuts` в `src/main.rs:233-243` происходит после
  `shortcut_started_for_app.replace(true)` на `src/main.rs:216`; повторная
  активация приложения уже не пытается зарегистрировать hotkeys заново.
- `src/main.rs:656-678` при закрытии явно останавливает recorder и preview,
  но не отправляет stop/cancel для shortcuts worker.

**Impact:** временный отказ portal, недоступный window identifier или
  disconnect оставляет приложение без пути восстановления до полного
  перезапуска. При закрытии нет явной передачи владения и остановки фоновой
  portal-задачи, что усложняет предсказуемый shutdown и повторное открытие UI.

**Smallest remediation direction:** возвращать из регистрации небольшой
  `GlobalShortcutsHandle` с cancellation signal и non-blocking reaper; считать
  регистрацию завершённой только после ready-события, а при error сбрасывать
  startup guard и разрешать одну повторную регистрацию.

### TD-005 — AppImage build не воспроизводим и зависит от host GStreamer

**Priority:** P2

**Exact scope:** `scripts/build-appimage.sh:7-48` и дублированный runtime
plugin contract в `src/recorder.rs:548-600`, `src/preview.rs:35`,
`src/main.rs:709-731`.

**Evidence:**

- `scripts/build-appimage.sh:17-18` скачивает `linuxdeploy` и `appimagetool`
  из mutable `continuous` URLs без version pin и checksum/signature check.
- `scripts/build-appimage.sh:30-37` получает plugin `.so` из текущего host
  через `gst-inspect-1.0`; результат сборки зависит от установленных plugin
  versions и от наличия каждой factory на build machine.
- `scripts/build-appimage.sh:39-42` выбирает первый scanner через `find
  /usr/lib`, то есть также зависит от layout host filesystem.
- Список элементов поддерживается отдельно в build script
  (`scripts/build-appimage.sh:30`), в diagnostics (`src/main.rs:709-731`) и
  фактическими launch descriptions (`src/recorder.rs:548-600`,
  `src/preview.rs:35`). В script/diagnostics всё ещё присутствуют
  `cairooverlay` и `compositor`, тогда как tests подтверждают отсутствие этих
  ветвей в recorder на `src/recorder.rs:1120-1142`.

**Impact:** одинаковый commit может собрать разные AppImage или не собраться
  на чистой машине; omission/несовместимая host factory обнаруживается только
  на этапе сборки или запуска packaged runtime. Ручной drift между pipeline,
  diagnostics и bundle list способен оставить release без нужного plugin.

**Smallest remediation direction:** закрепить версии и checksums внешних
  packaging tools, а список runtime factories сделать единственным явным
  manifest-источником, из которого проверка и bundling получают одинаковый
  набор. Host GStreamer должен быть частью явно зафиксированной build
  environment, а не неявной зависимостью.

### TD-006 — `--diagnose` не является read-only диагностикой

**Priority:** P2

**Exact scope:** порядок startup initialization в `src/main.rs:152-166` и
  реализация desktop entry в `src/portal.rs:8-37`.

**Evidence:**

- `src/main.rs:152-154` вызывает `ensure_desktop_entry()` до проверки
  `--diagnose` на `src/main.rs:155-157` и до `--portal-test` на
  `src/main.rs:158-165`.
- `src/portal.rs:8-37` создаёт каталог и перезаписывает
  `~/.local/share/applications/com.serg.livescreen.desktop` при каждом таком
  запуске.

**Impact:** команда, которую README предлагает для диагностики
  (`README.md:11-15`), меняет пользовательское состояние. Более того,
  permission/read-only failure в data directory прерывает диагностику до
  вывода сведений об окружении, поэтому инструмент не может диагностировать
  часть собственных startup-проблем.

**Smallest remediation direction:** разобрать диагностические флаги до
  side-effectful initialization и сделать `--diagnose` полностью read-only;
  регистрацию desktop entry оставить только обычному запуску и отдельному
  portal smoke path.

### TD-007 — ключевые runtime boundaries не покрыты автоматической проверкой

**Priority:** P2

**Exact scope:** тестовая стратегия для portal, PipeWire/devices, packaged
  GStreamer и итогового MP4; `src/*` tests, `README.md:65-67`,
  `index-codebase.md:137`.

**Evidence:**

- `cargo test` сейчас проходит 23 unit-теста; recorder tests проверяют
  parseability descriptions и синтетические lifecycle helpers
  (`src/recorder.rs:1120-1142`, `src/recorder.rs:1255-1363`), но не создают
  реальный portal-backed recording.
- `README.md:65-67` прямо сообщает, что автоматические проверки не создают
  видео через Wayland portal и не выполняют `ffprobe`.
- `index-codebase.md:137` подтверждает отсутствие проверки реального файла,
  длительности и codec streams; в исходном дереве нет отдельного CI/workflow
  или runtime smoke harness.

**Impact:** зелёные локальные gates не обнаружат поломку пользовательского
  разрешения portal, PipeWire negotiation, device source, bundled plugin или
  фактической H.264/AAC/MP4 финализации. Дефект может дойти до packaged
  artifact и обнаружиться только вручную.

**Smallest remediation direction:** добавить один opt-in environment-gated
  runtime smoke для recording path и postcondition через `ffprobe` (streams,
  duration, non-zero finalized MP4), запускать его в release/package check,
  сохранив unit-тесты для изолированной state machine.

### TD-008 — application shell остаётся высокосвязанным и плохо тестируемым

**Priority:** P3

**Exact scope:** `src/main.rs:152-685`; GTK construction, preview, recorder,
  hotkeys, status rendering и close lifecycle находятся в одном
  `connect_activate` closure.

**Evidence:**

- `src/main.rs:206-244` запускает hotkeys, `:246-457` строит всю форму и
  overlay/preview wiring, `:459-553` связывает recorder callbacks, а
  `:555-680` содержит event polling, hotkeys polling и shutdown.
- Файл содержит 768 строк, при этом unit-тесты `src/main.rs:737-768`
  покрывают только чистые helper-функции; GTK lifecycle и cross-resource
  shutdown не имеют тестируемого seam.
- `FINDINGS_stack.md:63-73` фиксирует тот же текущий coupling как отдельный
  unresolved maintenance debt.

**Impact:** изменение любой границы между GTK, recorder, preview или hotkeys
  требует правки большого callback-графа и ручной проверки всего lifecycle;
  ошибки порядка остановки и UI-state rendering трудно ловить unit-тестами.

**Smallest remediation direction:** выделить только application-shell adapter
  (создание виджетов, обработка доменных событий и shutdown coordinator) с
  чистыми входами/выходами для тестов; оставить `main.rs` composition root и
  не дробить вертикальные срезы повторно.

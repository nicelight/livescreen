# Индекс кодовой базы LiveScreen

Навигационный handoff для coding-агентов. Здесь описаны только связи, границы ответственности и неочевидные runtime-контракты. Назначение проекта, правила работы и quality gates находятся в `AGENTS.md`.

`FINDINGS_stack.md` содержит приоритизированные известные проблемы надёжности; открывай его, если задача касается ошибок записи, конфигурации или тестового покрытия.

## Как ориентироваться

Сначала найди вертикальный срез, к которому относится задача, затем открывай его точку интеграции. Не восстанавливай архитектуру чтением всего `src/main.rs` и не меняй соседний срез без необходимости.

| Задача | Вертикальный срез | Основные точки интеграции |
|---|---|---|
| JSON-настройки и defaults | configuration | `PortalConfig`, `UserSettings`, `Paths`, `read_json`, `write_json` |
| логи и диагностика | logging | `logging::init`, `LIVESCREEN_LOG`, `LIVESCREEN_LOG_DIR` |
| список камеры и аудио | devices | `camera_devices`, `audio_sources` |
| camera preview | camera preview | `PreviewFrame`, `PreviewController` |
| desktop кружочек | camera overlay | `CameraOverlay`, `create_camera_overlay`, `draw_overlay_camera` |
| прозрачность | camera overlay | `Animation`, `OverlayOpacityState`, `CameraOverlay` |
| экранный отсчёт и статусы записи | recording indicator overlay | `RecordingIndicatorOverlay`, `create_recording_indicator_overlay`, `IndicatorAnimation` |
| screen/audio recording | recorder | `RecorderOptions`, `build_pipeline`, `run_recorder` |
| stop/pause/lifecycle записи | recorder | `RecorderState`, `RecorderController`, `RecorderHandle`, `RecorderCommand`, `RecorderEvent` |
| Wayland screen permission | portal | `portal_connection`, `portal_stream`, `ensure_desktop_entry` |
| global hotkeys | portal hotkeys | `start_global_shortcuts`, `ApplicationCommand`, `WindowIdentifier`, command channel |
| GTK UI и shutdown | application shell | `main`, GTK timers и callbacks |
| AppImage runtime | packaging | `data/`, `scripts/build-appimage.sh`, `data/AppRun` |

## Архитектурные инварианты

- Камера не входит в recording pipeline. Overlay — отдельное окно; запись захватывает экран, уже скомпонованный compositor'ом.
- GTK objects и Layer Shell окна создаются и изменяются только в GTK main thread.
- `PreviewController` владеет camera preview thread; `run_recorder` владеет recording pipeline и EOS.
- Worker-потоки не обращаются к GTK напрямую: используют `std::sync::mpsc`; локальный atomic-арбитр recorder задаёт только результат операции запуска, но не дублирует доменный lifecycle.
- Wayland screen capture проходит через пользовательское разрешение xdg-desktop-portal/PipeWire.
- Overlay в `selfie OFF visible` и во время перехода не должен блокировать клики: `set_can_target(false)` и пустой input region — часть контракта. В полностью активном `Selfie ON visible` overlay принимает левый drag и click только внутри круга; на время drag input region расширяется до всей фиксированной поверхности.
- Stop не должен блокировать GTK main loop; после подтверждённого запуска он завершает MP4 через EOS, а во время `Starting` отменяет запуск и переводит pipeline в `Null` без EOS.
- Новые GStreamer elements требуют синхронного обновления списка plugins в `scripts/build-appimage.sh`.

## Вертикальные срезы в `src/`

`src/main.rs` — composition root: bootstrap процесса, GTK application shell, создание виджетов, lifecycle и связывание доменных каналов. Каждый вертикальный срез физически выделен в отдельный модуль, но остаётся частью одного бинарного crate. Новая slice-specific логика должна попадать в соответствующий существующий модуль; в `main.rs` остаются только создание и связывание компонентов. Новый модуль оправдан отдельным владельцем ресурса/lifecycle, независимо тестируемой доменной логикой или необходимой изоляцией от GTK main thread, но не размером файла. Одноразовые GTK callbacks не выносятся, если это требует длинных списков аргументов, лишних `clone()` или общего контейнера состояния.

### Configuration

`src/configuration.rs` содержит `PortalConfig`, `UserSettings`, `CameraPosition`, `CameraQuality`, `Paths`, загрузку и атомарное сохранение JSON. Новые поля используют `serde(default)`, старые валидные конфиги продолжают читаться; качество камеры по умолчанию — `Optim`, а отсутствующий `camera_border_visible` сохраняет прежнюю включённую рамку. `UserSettings` хранит `record_hotkey`, `stop_hotkey` и `mute_hotkey`; для старых settings используются defaults `Ctrl+Alt+F` и `Ctrl+Alt+M`. Отсутствующий JSON даёт defaults без warning; повреждённый или невалидный JSON даёт defaults с warning для GTK UI. Проверяются прозрачность `0..=100`, размер камеры `5..=50`, переход `0..=60_000` мс, непустые hotkeys и output directory. Запись идёт во временный файл в том же каталоге с последующим `rename`.

Ошибка чтения файла, отличная от `NotFound`, возвращается с контекстом. Ошибка сохранения `settings.json` при начале записи останавливает обработчик до запуска recorder.

### Logging

`src/logging.rs` инициализирует единый `tracing` subscriber до запуска GStreamer и GTK. События выводятся в stderr и в ежедневный файл `$XDG_DATA_HOME/livescreen/logs/livescreen.log.YYYY-MM-DD` (`~/.local/share` используется по умолчанию); каталог можно изменить через `LIVESCREEN_LOG_DIR`, уровень — через `LIVESCREEN_LOG` или `RUST_LOG`. Дата в имени и timestamp формируются в UTC. Файловый writer работает асинхронно и живёт до завершения `main`, а недоступность каталога логов не блокирует запуск приложения. `scripts/run-debug.sh` оставляет GStreamer на уровнях state/error (`pipewiresrc:4`) и отключает подробный PipeWire trace, чтобы не писать сообщения для каждого кадра.

Recorder пишет этапы `portal`, `pipeline`, `prepared`, `playing` и `stopping`, связывая их через `RecordingSessionId` и `elapsed_ms`. `prepared` означает ожидание GTK countdown до `BeginRecording`; для подтверждения `Started` логируются результат `set_state(Playing)`, последовательность `current/pending` и сообщения GStreamer Bus. GTK отдельно пишет получение и применение событий worker.

### Devices и camera preview

`src/devices.rs` содержит `camera_devices` и `audio_sources`: перечисляет `/dev/video*`, пытается исключить служебные video-ноды через `v4l2-ctl`, получает PulseAudio/PipeWire sources через `pactl` и всегда добавляет default entries.

`src/preview.rs` содержит `PreviewController` и `PreviewFrame`. Контроллер запускает отдельный GStreamer pipeline V4L2 → JPEG decode → RGBA `appsink`; профиль `CameraQuality` задаёт входной режим камеры, размер RGBA-кадра и частоту. `Best` использует `1280×720` → `640×360` при 30 fps, `Optim` — `640×360` → `480×270` при 30 fps, `Fast` — `640×360` → `320×180` при 30 fps. Входные caps должны соответствовать дискретным режимам конкретной V4L2-камеры; на поддерживаемой камере `Fast` уменьшает только output. Кадры передаются через channel в GTK и используются только desktop overlay; окно настроек живого превью не содержит. При смене качества или камеры текущий worker получает stop, а новый supervisor-поток сначала дожидается его завершения и только потом открывает новый V4L2 pipeline; это исключает гонку `Device or resource busy` на `/dev/video0` и не блокирует GTK main thread. Обычная остановка передаёт `JoinHandle` background reaper thread; worker сам переводит pipeline в `Null`. Последний кадр overlay сохраняется до прихода первого кадра нового профиля. Отсутствие камеры не должно приводить к panic.

### Desktop overlays

`src/overlay.rs` содержит `CameraOverlay`, `create_camera_overlay`, animation и Cairo drawing. Это отдельное GTK4 окно. На Wayland оно использует GTK4 Layer Shell `overlay` layer, нулевую exclusive zone и фиксированную полноэкранную поверхность, привязанную ко всем четырём краям выбранного монитора. Круг камеры рисуется внутри этой поверхности по координатам `OverlayGeometry`; положение самого Layer Shell окна во время drag не меняется. `create_camera_overlay` связывает окно с shared camera frame.

На X11 окно камеры имеет размер круга, вычисленный по ширине монитора. Круг рисуется в локальных координатах окна, а перетаскивание после порога движения передаётся оконному менеджеру через GDK `begin_move`. Начальное размещение определяет оконный менеджер. После показа камеры и индикатора записи `present_overlay` запрашивает для каждого X11-окна состояние `_NET_WM_STATE_ABOVE`, чтобы оба оставались поверх обычных окон при смене фокуса.

Положение, размер, видимость рамки и два значения прозрачности изменяются из UI; рамка рисуется Cairo только при включённом `camera_border_visible`. `Animation` и `OverlayOpacityState` поддерживают переход между `Selfie ON visible` и `selfie OFF visible`, включая разворот от текущего значения при повторном hotkey. В `Selfie ON visible` `CameraOverlay` принимает левый drag и клик; drag меняет координаты круга внутри монитора, а click без движения вызывает тот же toggle, что кнопка и hotkey. Правая кнопка вызывает тот же чувствительный `START/PAUSE/RESUME` GTK action, что основная кнопка; во время перехода и OFF обе mouse-команды недоступны. Для обычного состояния input region строится как круговая область, во время drag временно используется вся фиксированная поверхность, а после drag область снова ограничивается кругом. Это предотвращает feedback loop и рывки, которые возникали при изменении Layer Shell margins на каждом движении мыши. При начале перехода input region очищается, а после полного возврата в `Selfie ON visible` интерактивность включается снова. Перетянутая позиция действует до завершения текущего запуска; начальная позиция и размер остаются настройками.

`RecordingIndicatorOverlay` — второе, независимое полноэкранное GTK4 Layer Shell окно на слое `Overlay`. Оно создаётся в GTK main thread только после `RecorderEvent::Prepared`, получает размер монитора и широкие центрированные метки, имеет `KeyboardMode::None`, нулевую exclusive zone, пустой input region и не принимает GTK targets. Поэтому надписи не обрезаются, а клики проходят к нижележащим окнам. Две метки и GTK frame clock показывают тестируемую временную шкалу `2 → 1 → Rec`, салатово-жёлтый fade `Paused`, красный 200-миллисекундный `Rec` после Resume и зелёный секундный fade `STOP`. Между сигналами поверхность прозрачна. Команда Stop отправляется recorder немедленно, а indicator-layer закрывается после завершения секундного `STOP`; terminal cleanup не обрывает этот сигнал. Ошибка, отмена без Stop и закрытие приложения закрывают окно сразу. Для следующей recording session окно создаётся заново, camera overlay не затрагивается.

Текущая multi-monitor особенность: overlay привязан к первому GDK-монитору, а ScreenCast portal предлагает монитор отдельно и интерактивно.

### Recorder

`src/recorder.rs` содержит `RecorderState`, единственную `RecorderStateMachine` внутри `RecorderController`, `RecorderOptions`, `RecorderHandle`, команды/события lifecycle, `build_pipeline` и `run_recorder`. `run_recorder` остаётся worker-orchestrator и последовательно вызывает внутренние фазы `acquire_portal_capture`, `start_recording_pipeline`, `run_recording_commands`, `finalize_recording_pipeline` и `finish_recording_output`; эти функции не владеют отдельным lifecycle и не образуют вторую state machine. Дочерний unit-test модуль `src/recorder/tests.rs` сохраняет доступ к приватным элементам recorder через `super` и не является integration test. Recorder строит прямой screen-only MP4 pipeline без промежуточного MKV:

```text
Wayland: PipeWire screen source ─┐
X11:     ximagesrc              ├─ video → H.264 → mp4mux
microphone + system monitor ────┴─ audiomixer → AAC → mp4mux
```

Wayland `pipewiresrc` получает restricted FD и node ID от ScreenCast portal. Сразу после сборки pipeline `configure_pipewire_screen_source` обязательно задаёт ему `media.type=Video`, `media.category=Capture` и `media.role=Screen`. Эти свойства являются частью контракта построения Wayland pipeline: без них WirePlumber может классифицировать source как `Stream/Input/Unknown` и не связать его с выбранным portal video node. Не удаляй эту настройку при изменении caps или перестройке pipeline.
Для статичного экрана Wayland source использует `keepalive-time=33`: PipeWire повторяет последний кадр примерно каждые 33 мс, поэтому видеодорожка не заканчивается раньше аудио только из-за отсутствия изменений на экране.

#### Корректный lifecycle recorder и Wayland Pause/Resume

`RecorderController` — единственный владелец доменного lifecycle со состояниями `Idle`, `Starting`, `Recording`, `Paused`, `Stopping`, `Error`. Worker не содержит второй state machine: он выполняет команды и возвращает `Prepared`, подтверждённые `Started`, `Paused`, `Resumed` либо terminal event. `Prepared` не меняет состояние: pipeline уже собран, но остаётся в `Null`, пока GTK не завершит отсчёт и не отправит `BeginRecording`. Все команды и события несут `RecordingSessionId`, поэтому запоздалое событие старой сессии не может изменить новую.

Алгоритм, порядок которого нельзя менять без повторной реальной Wayland-проверки:

1. **Start.** Controller создаёт worker и переходит в `Starting`. На Wayland worker получает через ScreenCast portal session, restricted PipeWire FD и node ID; portal runtime, stream и FD должны жить дольше recording pipeline.
2. **Pipeline startup.** Worker строит прямой MP4 pipeline без camera branch, `cairooverlay`, `compositor`, output valves, MKV и remux. `pipewiresrc` использует `keepalive-time=33 provide-clock=false`, все `pulsesrc` — `provide-clock=false`, а `filesink` — `async=false`. Микрофонный `pulsesrc` отдельно именуется `microphone_source`; его штатное свойство `mute` задаёт начальное состояние и меняется командой без остановки pipeline, не затрагивая системный звук.
3. **Prepared и countdown gate.** После получения Bus worker отправляет `Prepared` и ждёт `BeginRecording`, оставаясь в `Starting`. GTK создаёт session-scoped indicator layer и выполняет двухсекундную анимацию. `Stop` на этом этапе немедленно отменяет startup и переводит pipeline в `Null` без EOS; indicator layer показывает секундный `STOP` и закрывается после fade. Команды Mute, пришедшие во время ожидания, применяются к подготовленному микрофонному source.
4. **Commit запуска.** После `BeginRecording` worker вызывает `set_state(Playing)`, bounded-проверкой подтверждает `current=Playing` и `pending=VoidPending`, проверяет Bus Error и только затем выполняет единственный CAS startup-арбитра `Pending → Committed`. После commit отправляется `Started`, а Controller переходит в `Recording`.
5. **Pause.** Команда допустима только из `Recording`. Worker переводит весь pipeline в `Paused`; для live pipeline `GST_STATE_CHANGE_NO_PREROLL` является нормальным результатом. `Paused` публикуется только после подтверждения `current=Paused`, `pending=VoidPending`.
6. **Resume.** Пока pipeline находится в `Paused`, `reset_pipewire_source_before_resume` переводит только Wayland `screen_source` в `Ready` и подтверждает это состояние. Затем весь pipeline переводится в `Playing`; `Success` и промежуточный `Async` допустимы, но `Resumed` отправляется только после фактического `current=Playing`, `pending=VoidPending`.
7. **Stop после commit.** Controller сразу переходит в `Stopping`. Если pipeline был `Paused`, worker сначала выполняет тот же Resume-path. Затем EOS отправляется на подключённые sink pads `mp4mux`, ожидается Bus EOS, pipeline переводится в `Null`, и итоговый путь проверяется как существующий непустой обычный MP4. Только после всех этих шагов публикуется `Stopped`.
8. **Stop до commit.** Startup-арбитр фиксирует отмену, будит ожидание portal/Prepared/Playing, pipeline при наличии переводится в `Null` без EOS, а созданный этой сессией zero-byte placeholder удаляется. Результат — `Cancelled`, не `Stopped`. При одновременно готовых portal result и отмене первым разбирается portal result: `ResponseError::Cancelled` остаётся пользовательской отменой, независимая portal-ошибка — `Error`.
9. **Ошибки.** Ошибки portal, `set_state`, Bus, EOS, `Null` и проверки файла не скрываются и возвращаются как session-correlated `RecorderEvent::Error`. Bus Error, уже наблюдавшийся первым, не переписывается последующей отменой; принятая первой отмена не переписывается поздней ошибкой.

Финальная последовательность Pause/Resume/Stop:

```text
Recording
  → pipeline Paused
  → pipewiresrc Ready
  → pipeline Playing
  → EOS в mp4mux sink pads
  → Bus EOS
  → pipeline Null
  → проверка MP4
  → Stopped
```

Этот порядок получен после длительной диагностики двух независимых дефектов системного PipeWire 1.6.2:

- Первая реализация Pause через output valves оставляла live sources и encoder в `Playing`, но после Resume видеодорожка зависала или обрывалась; valves проблему не устранили и были удалены. Pipeline-wide `Playing → Paused → Playing` оставлен единственным механизмом паузы.
- С `filesink async=true` live pipeline после Resume мог оставаться в `Paused` с `pending=Playing`; `filesink async=false` убрал зависимость перехода от preroll файлового sink.
- После успешного Resume source-provided PipeWire clock пропускал первый video buffer, а следующий buffer с PTS примерно `+33 ms` удерживал внутри `GstBaseSrc` около пяти секунд. `videorate` дублировал редкие кадры, поэтому пользователь видел статичное видео. Все capture sources получили `provide-clock=false`, после чего pipeline использует системные часы GStreamer.
- Системный `gstpipewiresrc` мог оставить внутренний `flushing=true` после `Playing → Paused`, из-за чего `Paused → Playing` возвращал ошибку. Переход `screen_source: Paused → Ready` перед Resume запускает штатный `GstBaseSrc` stop/unlock-stop и очищает это состояние. Локально собранный plugin применялся только для изоляции причины и не является зависимостью приложения.

До исправления `screen-20260807-225738.mp4` содержал только pre-pause материал, а `screen-20260807-231256.mp4` и визуальный таймер подтвердили последовательность «кадр сразу после Resume → следующий кадр через ~5 секунд». После исправления записи без паузы, с одной и двумя паузами успешно прошли на штатном системном plugin; video/audio durations совпали, H.264/AAC присутствовали, максимальное ожидание video buffer составило 60 мс, GStreamer Error и EOS timeout отсутствовали.

`RecorderHandle::Drop` не делает blocking join в GTK callback: recorder thread передаётся отдельному join-thread. Ожидание portal, GStreamer state, EOS и `JoinHandle` выполняется только вне GTK main thread.

### Portal и global hotkeys

`src/portal.rs` содержит ScreenCast connection/stream и регистрацию desktop entry. `portal_connection` открывает session D-Bus connection и регистрирует host app id `com.serg.livescreen`. `ensure_desktop_entry` создаёт пользовательский desktop entry с фактическим executable, чтобы portal мог идентифицировать приложение. `SelectSourcesOptions` объявляет только `SourceType::Monitor` и `multiple=false`: Wayland portal сохраняет обязательное разрешение/выбор монитора, но не предлагает отдельные окна или virtual sources.

`src/shortcuts.rs` содержит `start_global_shortcuts`: регистрирует четыре portal shortcuts — запуск/пауза `Ctrl+Alt+R`, остановка `Ctrl+Alt+F`, `Selfie ON/OFF` `Ctrl+Alt+O` и Mute микрофона `Ctrl+Alt+M`. Parent window сначала показывается и экспортируется в `WindowIdentifier`; поток подписывается на `Activated` до bind и логирует фактически принятые portal triggers. Команды hotkey передаются в GTK main thread, где сопоставляются с текущим `RecorderState` или переключают overlay/Mute. Любая отображаемая кнопка hotkey отправляет в тот же поток запрос `ConfigureShortcuts`; portal открывает стандартное окно KDE для всей сессии. После закрытия окна поток читает новые bindings, получает также `ShortcutsChanged` для внешних изменений и передаёт `ShortcutUpdate` в GTK, где подписи и JSON обновляются. Системное окно Global Shortcuts Requested принадлежит KDE/portal и не встраивается в GTK UI; оно используется для первичного разрешения, а последующее редактирование выполняется через `ConfigureShortcuts`. `RecorderCommand` используется worker-потоком recorder для `BeginRecording`, `Stop`, `Pause`, `Resume` и изменения Mute микрофона.

### Application shell

`src/main.rs` связывает вертикальные срезы: загружает конфигурацию, обнаруживает устройства, создаёт settings window и camera overlay, запускает preview, регистрирует hotkeys и централизованно отображает состояния/события recorder через GTK timers. По `Prepared` GTK создаёт отдельный `RecordingIndicatorOverlay`, запускает countdown и по его завершении отправляет `BeginRecording`; по подтверждённым `Paused`/`Resumed` показывает соответствующие сигналы. Session-scoped `Rc<RefCell<Option<...>>>` используется только для GTK-владения окном и не дублирует recorder lifecycle. Главное окно по умолчанию компактное по высоте: наверху находятся три крупные круглые кнопки `START/PAUSE/RESUME`, `STOP`, `SELFIE` и постоянно видимый статус. Маленький toggle `MUTE` расположен через `GtkOverlay` в правом нижнем углу верхнего блока, поэтому не участвует в расчёте его размера; статус имеет безопасный правый отступ. Низкий широкий label «Настройки» штатного `GtkExpander` раскрывает внутри окна полную форму без внутренней прокрутки. Нересайзабельное top-level окно имеет постоянную ширину раскрытой формы и естественную высоту без фиксированного default height; `resize-toplevel` меняет только его высоту по видимому содержимому. Выбор начального положения камеры в UI отсутствует: overlay использует обратно совместимое значение `camera_position` из JSON и далее перемещается мышью. Компактный checkbutton «Рамка» делит строку качества Selfie и не добавляет форме высоту; значение применяется live и сохраняется с `UserSettings`. Интерактивные подписи четырёх hotkeys собраны в уплотнённую сетку 2×2; локальные CSS-классы убирают стандартный вертикальный padding hotkey-кнопок и делают рамку `SELFIE` полупиксельной и приглушённой, не меняя остальные controls. Подписи открывают portal `ConfigureShortcuts`, обновляются событиями portal и сохраняют выбранные комбинации. Основная кнопка имеет действие `START` в `Idle`, `PAUSE` в `Recording` и `RESUME` в `Paused`; CameraOverlay right-click делегирует этому же action только пока кнопка чувствительна. Отдельная кнопка `STOP` доступна для активной сессии, включая portal selection и countdown. После принятого Stop GTK сразу отправляет recorder-команду и показывает секундный зелёный `STOP`; indicator-layer закрывается после fade, не задерживая EOS. Controller state `Stopping` сразу блокирует кнопки управления и показывает `сохранение видео, ждите..` до `Stopped/Error/Cancelled`. При закрытии окна активный recorder получает Stop, но GTK имеет 10-секундный bounded fallback, чтобы не блокировать крестик бесконечно при зависшем внешнем pipeline. Timer нужен только для доставки worker events, а не для корректности отображения принятой команды. Внешние статусы hotkey не могут перезаписать этот статус, пока recorder находится в `Stopping`.
Тесты slice-specific поведения находятся рядом с соответствующими модулями; `main.rs` не дублирует тесты recorder или других срезов.

Lifecycle в сжатом виде:

```text
main → config/devices → GTK settings controls + CameraOverlay + PreviewController
record command → RecorderController → portal/pipeline → Prepared → RecordingIndicatorOverlay countdown → BeginRecording → Started
opacity command → GTK main thread → CameraOverlay animation
Pause/Resume confirmation → RecordingIndicatorOverlay signal
Stop → recorder немедленно + STOP fade 1 с → destroy indicator; window close → destroy overlays + stop recorder/preview
```

## Владение ресурсами и каналы

| Ресурс | Владелец | Граница остановки |
|---|---|---|
| camera GStreamer pipeline | `PreviewController` в `preview.rs` | stop channel → worker `State::Null` → background join |
| shared camera frame | GTK `Rc<RefCell<Option<PreviewFrame>>>` для overlay | preview thread → channel → GTK timer |
| overlay window/area | `Rc<CameraOverlay>` в GTK callbacks/timers | GTK main thread; фиксированная полноэкранная Layer Shell surface, координаты круга в `OverlayGeometry`, круговой input region и click-through при OFF/transition |
| recording indicator layer | session-scoped `Rc<RecordingIndicatorOverlay>` в GTK main thread | создаётся по `Prepared`; frame-clock animation; полноэкранная allocation и пустой input region; Stop закрывает после секундного fade, error/cancel без Stop и shutdown — сразу |
| recorder GStreamer pipeline | `run_recorder` в `recorder.rs` | cancellable `Playing` confirmation → единый startup commit либо `Cancelled/Error`; Pause переводит весь pipeline в `Paused`, Resume делает Wayland source reset `Ready` и возвращает pipeline в `Playing`, capture sources не предоставляют pipeline clock; отмена → bounded `State::Null` без EOS → безопасная очистка placeholder; Stop из Paused → Resume path → EOS на подключённые sink pads `mp4mux` → Bus wait с приоритетом доступного Error → `State::Null` → file check |
| PipeWire remote FD, ScreenCast session и portal runtime | recorder thread | `PortalStream` и runtime живут до завершения recorder pipeline; затем pipeline закрывается раньше portal-ресурсов |
| recorder lifecycle and transport | `RecorderController` + `RecorderHandle` | terminal event → controller releases handle; background join при Drop |
| global shortcut stream and configuration channel | `shortcuts` thread/runtime | portal disconnect/error; GTK sends configure requests, portal returns binding updates |

Не передавай `gtk::Window`, `gtk::DrawingArea` или другие GTK objects в worker closure. Новый результат фоновой работы должен приходить доменным событием через channel и обрабатываться в GTK main thread.

## Runtime-контракты, которые легко нарушить

- Если камера отсутствует, UI использует пункт `Камера не найдена`, а preview не запускается.
- Если audio sources не обнаружены, остаются default entries; фактическая ошибка может проявиться только при запуске GStreamer.
- Если portal отменён, recorder отправляет `Cancelled`; ошибка portal отправляется как `Error`. UI возвращает кнопки в исходное состояние и не показывает «Файл сохранен».
- Stop во время portal selection должен прерывать ожидание и выдавать `Cancelled`, а не ждать portal или `JoinHandle` в GTK.
- Stop во время countdown должен сразу отменять startup gate и завершаться `Cancelled` без EOS; indicator-layer ещё одну секунду показывает `STOP`, не перехватывая ввод, затем закрывается.
- Закрытие окна удерживает GTK lifecycle до session-correlated `Stopped`, `Error` или `Cancelled`; после terminal event окно закрывается повторно через GTK main thread.
- Stop preview при смене камеры или закрытии окна не должен ждать `JoinHandle` в GTK; ожидание выполняет background reaper.
- Если Layer Shell не поддержан, показывается понятный статус; камера не должна незаметно возвращаться в post-processing pipeline.
- Hotkeys могут быть недоступны из-за desktop entry или portal permissions, но ручное управление UI должно продолжать работать. Настройка любой комбинации открывает один стандартный portal dialog для всей сессии; нельзя регистрировать отдельную portal-сессию на каждый label.

## Интеграционные связи файлов

- `Cargo.toml` определяет crates для `src/main.rs`; `Cargo.lock` генерируется Cargo и вручную не редактируется.
- `src/logging.rs` является общей точкой инициализации `tracing`; новые фоновые ошибки должны использовать `tracing` macros вместо `println!/eprintln!`.
- `src/gst_utils.rs` содержит общий escaping helper для GStreamer descriptions; он не зависит от recorder или preview.
- `src/portal.rs` встраивает `data/com.serg.livescreen.desktop` через `include_str!`; это имя и app id должны оставаться согласованными.
- `scripts/build-appimage.sh` собирает release binary, создаёт staging tree, копирует `data/` и bundles runtime plugins. `scripts/run-debug.sh` запускает AppImage/debug binary с application logs, целевым `GST_DEBUG`, GStreamer log file и DOT-графами pipeline.
- `data/AppRun` задаёт `GST_PLUGIN_PATH`, `XDG_DATA_DIRS` и `GST_PLUGIN_SCANNER` для packaged launch.
- `data/com.serg.livescreen.desktop` и `data/com.serg.livescreen.svg` являются исходными packaging assets.
- `README.md` — публичная витрина и краткий пользовательский старт; `docs/livescreen-window.png` — его актуальный UI-скриншот. `.gitignore` исключает сборочные, packaging и media-артефакты, а `LICENSE` фиксирует заявленную в `Cargo.toml` MIT-лицензию.
- Для пользовательского меню Linux desktop entry устанавливается как `~/.local/share/applications/com.serg.livescreen.desktop`, иконка — как `~/.local/share/icons/hicolor/scalable/apps/com.serg.livescreen.svg`; в текущей установке `Exec` и `TryExec` указывают на `~/.local/bin/LiveScreen-x86_64.AppImage` (абсолютный путь). Обновление заменяет AppImage по этому пути и сохраняет app id `com.serg.livescreen`. Избранное рабочего стола — пользовательское runtime-состояние окружения, а не часть репозитория; в KDE Plasma его membership регистрируется через KActivities `ResourcesLinking`, а `kactivitymanagerd-statsrc` задаёт только порядок.
- Runtime state создаётся вне проекта: `~/.config/livescreen/config.json`, `~/.config/livescreen/settings.json`, `$XDG_DATA_HOME/livescreen/logs/` (`~/.local/share` по умолчанию), пользовательский desktop entry и MP4 в выбранной папке.

При изменении overlay проверяй Layer Shell z-order, monitor, click-through, положение, размер и opacity без активной записи. При изменении portal/desktop entry проверяй app id и регистрацию host application. При изменении pipeline проверяй element factories и bundled plugins.

## Известные ограничения

- Основной поддерживаемый сценарий — Ubuntu/KDE Wayland с GTK4 Layer Shell; X11 overlay остаётся best-effort fallback.
- Multi-monitor выбор overlay и portal пока не связан.
- Автоматические тесты проверяют animation, configuration loading/validation/atomic write, terminal recorder semantics и parseability pipeline, но не создают реальный файл через Wayland portal и не выполняют `ffprobe`.

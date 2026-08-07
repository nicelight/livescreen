Проблема Pause/Resume состояла из двух независимых дефектов системного `gstpipewiresrc`:

- source-provided PipeWire clock после Resume пропускал первый buffer, но блокировал следующий примерно на 5 секунд; все capture sources теперь используют `provide-clock=false`;
- PipeWire 1.6.2 мог оставить `flushing=true`; перед Resume `screen_source` переводится в `Ready`, затем весь pipeline — в `Playing`.

Output valves удалены. Запись остаётся прямой в MP4. Штатный системный plugin успешно прошёл записи без паузы, с одной и двумя паузами; максимальное ожидание video buffer составило 60 мс, H.264/AAC дорожки согласованы, EOS и `Null` завершились без ошибок.

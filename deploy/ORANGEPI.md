# Orange Pi: развёртывание и обслуживание

Хост: SSH alias `orangepi`, рабочая папка `~/fpv-dvr-organaiser`. Оригиналы: `/tank/fpv_videos`. PostgreSQL каталога — отдельный сервис Compose, Immich — существующий отдельный стек. Секреты в `.env`, `.immich`, `.tg-bot`; права файлов `0600`. Значения ключей в документацию и Git не включаются.

## Сборки

На Orange Pi **не запускать** `cargo build`, `docker build` и `docker compose build/up --build`. Основной `docker-compose.yml` не содержит `build`; override `docker-compose.build.yml` предназначен только для рабочей станции. На сервер загружаются готовые ARM64-образы. GitHub Actions workflow `ARM64 deployment image` подготавливает скачиваемый artifact без доступа к серверу и секретам бота; сам workflow пока не запускался.

Пример обновления из готового artifact, уже распакованного на рабочей станции:

```sh
scp fpv-catalog-arm64.tar.gz orangepi:/tmp/
ssh orangepi 'cd ~/fpv-dvr-organaiser && umask 077 && docker compose exec -T postgres pg_dump -U fpv -d fpv -Fc > pre-update.dump.local'
ssh orangepi 'docker tag fpv-dvr-organaiser-fpv-catalog:latest fpv-dvr-organaiser-fpv-catalog:rollback'
ssh orangepi 'docker load -i /tmp/fpv-catalog-arm64.tar.gz'
# Обновить checkout до того же Git commit, из которого собран образ.
ssh orangepi 'cd ~/fpv-dvr-organaiser && docker compose up -d --no-build'
ssh orangepi 'curl --fail http://127.0.0.1:8080/healthz'
```

Если обновление не прошло, для возврата приложения использовать образ `:rollback` через `FPV_CATALOG_IMAGE` в `.env` и `docker compose up -d --no-build`. Возврат после изменений схемы БД требует отдельной оценки совместимости и при необходимости восстановления dump; не восстанавливать БД поверх работающего приложения.

## Конфигурация Immich

Проверенная версия — **3.3.3**. Архив примонтирован в Immich как `/mnt/fpv:ro`, в приложение — `/srv/fpv/sorted:ro`. Сопоставление использует относительный путь, точное равенство `originalPath`, UUID external library, тип VIDEO, offline/trash. Сами originals не перемещаются и не копируются в upload storage Immich.

По согласованию с владельцем архива задано `IMMICH_SHOW_METADATA=true`: в Immich 3.2.2 и 3.3.3 `showMetadata=false` принудительно выключает скачивание originals. Ссылки позволяют download, запрещают upload, содержат только выбранные assets, действуют 7 дней. Метаданные могут быть видны любому держателю ссылки.

Минимальные permissions технического API key: `asset.read`, `sharedLink.read`, `sharedLink.create`, `sharedLink.delete`. Для публичного скачивания используется разрешение самой shared link; ключ не передаётся пользователю. Библиотека создаётся и сканируется средствами Immich, бот не получает administrative permissions.

## Проверки 16 октября 2026

- Все **118** видео архива приняты после SHA-256-проверки sync receipts; пропусков при сверке файлов и каталога нет. Полностью классифицированы **53**, не классифицированы **65**. Ошибок ffprobe и конфликтов изменения исходника не выявлено.
- У неклассифицированных видео: 34 неизвестных пилота, 28 путей без пары `аппарат_видеосистема`, 3 пути с неверной глубиной. Ошибки могут пересекаться: эти 28 путей дают одновременно unknown_device и unknown_video_system.
- `T1` и `Microsector` — plane, `Whoop` — quad; `N3` — OSD, `O4` и `O4 Pro` — без OSD. Пилот по умолчанию — владелец архива; explicit alias `xartik` также привязан к нему.
- Реальный публичный HTTPS smoke: точный asset, страница 200, allowDownload=true, allowUpload=false, showMetadata=true, один выбранный asset, скачивание Range 206 и совпадение первых 1024 байт с оригиналом. Временная ссылка отозвана, последующий доступ запрещён.
- Telegram `/start`, поиск и карточка проверены пользователем. Включены выдача, ежедневные и системные уведомления; журнал доставки подтверждает отправку первого дайджеста и системных уведомлений.
- Локально: **21 тест** с PostgreSQL 16 и ffmpeg, fmt, Clippy с `-D warnings`. Проверяется отзыв ссылки, если Immich вернул неверные flags, другой набор assets или неподходящий срок.
- ARM64 release binary и образ приложения собираются на рабочей станции из проверенного runtime, не на Orange Pi. SHA-256 бинарника проверяется после загрузки образа.
- Развёрнут код `e257622`, образ `sha256:0349b9937b52b640f9767b7abf5771ffcc93f8e65b0ae31b2d81a78e3e4610c2`, архитектура linux/arm64. SHA-256 запущенного бинарника: `3b76bbaaa3ac5a6df8996d10bda98a3d055bd54fd8d9d3a46004bcb06e871844`. Runtime-слои на сервере переиспользованы через `docker load`; сборка выполнялась локально. Health после обновления — HTTP 200; Rust CLI smoke lookup/create/validation/revoke на Immich 3.3.3 прошёл.
- Живой тест файлов выявил стандартный тайм-аут teloxide 17 секунд; отправка файла использует отдельный HTTP client с `TELEGRAM_UPLOAD_TIMEOUT=10m`. Тайм-аут задан в серверном `.env`.
- После обновления обе реальные выдачи файла `ugor_01.10.26/t1_n3/DJI_0145.MOV` владельцу через очередь завершились `done`, по одному файлу, с первой попытки: режимы Telegram video и document. Размер оригинала — 11 915 028 байт.

## Незавершённая настройка архива

Для `kostyanbtw` и `sergmi_sh` нужны реальные Telegram IDs и пользовательские записи; вымышленные IDs не создаются. Папки вида `averyan_home_whoop/n3/файл` не содержат ожидаемого уровня `аппарат_видеосистема`. Автоматически извлекать аппарат из имени события бот не пытается. Эти файлы видны администратору через `/status` как ошибки классификации; после уточнения справочников и исправления структуры проводится повторная классификация. Перемещения originals должны согласовываться с sync-helper и Immich.

Публичный административный fallback не включён: `FALLBACK_BASE_URL` пуст. Для него требуется отдельный HTTPS reverse proxy; обычная выдача больших видео работает через Immich.

По умолчанию дайджест в 08:00 Europe/Moscow, системные уведомления не чаще раза в час. `/alertinterval` меняет только системный интервал, `/notify` — дайджесты. Для диагностики использовать `/status`, `/jobs`, `/scan` и HTTP `/healthz`. Не публиковать полный `docker inspect`, `docker compose config` или транспортные ошибки с URL Telegram: они могут содержать секреты.

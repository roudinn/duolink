# Сервис DuoLink

Сервис на Rust принимает запросы по HTTPS через nginx, хранит зашифрованную почту в SQLite и отвечает на STUN Binding по UDP. Локальные потоки Sunshine и Moonlight связываются напрямую через QUIC.

`POST /signal/{topic}` принимает ключ каталога или зашифрованное сообщение. `GET /signal/{topic}/json` открывает подписку на NDJSON. Параметр `poll=1` возвращает только сохранённые записи. `since` принимает Unix-время в секундах или `all`. `GET /healthz` проверяет HTTP-сервис.

На VPS HTTP слушает `127.0.0.1:8705`, STUN слушает UDP 3478 и 3479. Внешнее имя `duolink.154.86.119.7.sslip.io` проходит через существующий SNI-маршрутизатор nginx на `127.0.0.1:9464`. Разрешение на STUN ограничено двумя UDP-портами. nginx использует отдельный сертификат Let's Encrypt с webroot и перезагрузкой конфигурации после продления.

Сборка и тесты из корня репозитория.

```sh
cargo test --locked --manifest-path server/Cargo.toml
cargo build --release --locked --manifest-path server/Cargo.toml
```

Сервис запускается через [duolink.service](deploy/duolink.service). Бинарник устанавливается в `/opt/duolink/duolink-server`. При обновлении сначала проверьте `nginx -t` и локальный `/healthz`. Данные сервиса хранятся в `/var/lib/duolink`. Каталог хранится семь дней, личная почта двенадцать часов, адреса гостя для NAT-пробивки сорок секунд. На общем VPS одновременно допускается до 128 подписок. База сохраняется при перезапуске.

[Зеркало](https://duolink.154.86.119.7.sslip.io/downloads/) отдаёт три закреплённых артефакта из `/srv/duolink/downloads`. При обновлении используйте [SHA256SUMS](deploy/SHA256SUMS), [страницу зеркала](deploy/downloads.html), оригинальные релизные бинарники и их лицензии. Драйвер Virtual HID Broker скачивается с сайта поставщика.

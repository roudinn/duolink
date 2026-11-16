# Сборка и свой сервер

## Windows-клиент

Понадобятся Rust 1.88 или новее, MSVC Build Tools с пакетом Desktop development with C++, Windows SDK и CMake в `PATH`. Для запуска окна нужен Microsoft Edge WebView2 Runtime.

```powershell
git clone https://github.com/roudinn/duolink.git
Set-Location duolink
git checkout v1.15.2
rustup default stable-x86_64-pc-windows-msvc
cargo build --release --locked
cargo test --release --locked
```

Готовый файл находится в `target\release\duolink.exe`. Для запуска без установки задайте `$env:DUOLINK_PORTABLE = '1'` в этом окне PowerShell. Чтобы отделить настройки сборки от обычного профиля, задайте `$env:DUOLINK_DATA_DIR = Join-Path (Get-Location) '.qa'`.

## Сервер на Linux

У сервера есть HTTPS для сигналинга и два UDP-порта для STUN. Для примера используем `link.example.net` и `203.0.113.10`. Замените их своим доменом и публичным IPv4. HTTP-бэкенд доступен локально на TCP 8705.

1. На VPS установите Rust 1.88 или новее, компилятор C и nginx. Направьте A-запись домена на VPS и получите TLS-сертификат для домена.
2. Укажите TCP 80/443 и UDP 3478/3479 в правилах VPS и панели провайдера. TCP 8705 оставьте на loopback.
3. Соберите бинарник и положите его по пути, указанному в unit-файле.

```bash
git clone https://github.com/roudinn/duolink.git
cd duolink
cargo test --locked --manifest-path server/Cargo.toml
cargo build --release --locked --manifest-path server/Cargo.toml
sudo install -D -m 0755 server/target/release/duolink-server /opt/duolink/duolink-server
sudo install -m 0644 server/deploy/duolink.service /etc/systemd/system/duolink.service
sudoedit /etc/systemd/system/duolink.service
```

4. Укажите в `DUOLINK_STUN_A` и `DUOLINK_STUN_B` адрес интерфейса VPS с портами 3478 и 3479. Если провайдер выдаёт публичный IP через NAT, здесь нужен локальный адрес, на который он перенаправляет оба UDP-порта. Для SQLite systemd подготовит `/var/lib/duolink`.
5. Возьмите [конфигурацию nginx](../server/deploy/nginx.conf) за основу. Замените имя хоста, редирект и пути к TLS-сертификату. Для отдельного VPS слушайте `443 ssl` вместо `127.0.0.1:9464 ssl`. Внутренний порт 9464 нужен схеме с общим маршрутизатором SNI. Проверьте конфигурацию и запустите сервис.

```bash
# После установки адаптированного файла в /etc/nginx/conf.d/duolink.conf
sudo systemctl daemon-reload
sudo systemctl enable --now duolink
sudo nginx -t && sudo systemctl reload nginx
curl --fail http://127.0.0.1:8705/healthz
curl --fail https://link.example.net/healthz
```

### Подключить свои клиенты

> [!IMPORTANT]
> Адреса сигналинга и STUN заданы при сборке. Чтобы оба клиента использовали свой VPS, измените `ORIGIN`, `SIGNAL_ADDRESS` и `STUN` в [src/infra.rs](../src/infra.rs), затем соберите и запустите новую копию на обоих ПК.

`SIGNAL_ADDRESS` имеет формат `домен:443:IPv4`. Домены STUN в [src/tunnel.rs](../src/tunnel.rs) используются для дополнительной диагностики NAT. Для собственной сборки установите `DUOLINK_NO_UPDATE=1` в окружении запуска, чтобы клиент не перешёл на официальный релиз с адресом публичного сервера.

Если свой VPS раздаёт компоненты, разместите файлы из [SHA256SUMS](../server/deploy/SHA256SUMS) в `/srv/duolink/downloads`. При недоступности зеркала клиент обращается к официальным релизам компонентов.

<details>
<summary>Проверка перед обновлением</summary>

Сохраните копии SQLite и файлов развёртывания. Для проверки 100 клиентов используйте отдельный экземпляр с временной базой.

```bash
node server/tests/capacity.mjs server/target/release/duolink-server
```

С другого компьютера проверьте HTTPS, STUN на обоих портах и игру между двумя Windows-ПК. [Руководство по эксплуатации сервера](../server/README.md)

</details>

## Свой ретранслятор

Используйте [серверный архив для Linux x86-64](https://github.com/roudinn/duolink/releases/latest/download/duolink-server-linux-x86_64.tar.gz) и проверьте его по `SHA256SUMS.txt` из релиза. На VPS с готовым доменом и TLS распакуйте архив, установите бинарник и настройте службу и nginx по шагам выше. Если нужен только ретранслятор, задайте `DUOLINK_STUN_A=127.0.0.1:0` и `DUOLINK_STUN_B=127.0.0.1:0` в unit-файле. Для этих UDP-портов внешние правила не нужны.

Перед запуском DuoLink 1.15.2 или новее задайте на обоих ПК одинаковый адрес:

```powershell
$env:DUOLINK_RELAY_URL = 'wss://link.example.net/relay'
& .\DuoLink.exe
```

Прямое подключение и сигналинг продолжают работать с обычными настройками клиента. Переходим на указанный сервер только при попытке ретрансляции.

## Сторонние компоненты

Уведомления для Inter, Opus и Opusic находятся в [licenses](../licenses). DuoLink скачивает закреплённые версии [Sunshine](https://github.com/SunshineStream/Sunshine), [Moonlight](https://github.com/moonlight-stream/moonlight-qt) и [ViGEmBus](https://github.com/nefarius/ViGEmBus) и проверяет SHA-256 перед установкой.

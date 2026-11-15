**Русский** | [English](README.en.md)

<div align="center">
  <h1>DuoLink</h1>
  <p>Локальный кооператив и versus через интернет на Windows.</p>
  <p>
    <img alt="Windows 10 и 11, x64" src="https://img.shields.io/badge/Windows-10%20%2F%2011%20x64-303036?style=flat-square">
    <a href="https://github.com/roudinn/duolink/releases/latest"><img alt="Последний релиз" src="https://img.shields.io/github/v/release/roudinn/duolink?style=flat-square&amp;color=303036"></a>
    <img alt="Rust 1.88+" src="https://img.shields.io/badge/Rust-1.88%2B-303036?style=flat-square">
  </p>
  <h3><a href="https://github.com/roudinn/duolink/releases/latest/download/DuoLink.exe">Скачать DuoLink для Windows</a></h3>
  <p><a href="https://github.com/roudinn/duolink/releases/latest">Портативная версия и список изменений</a></p>
  <img src="docs/preview.png" width="1000" alt="Лобби DuoLink с гостем и форма подключения по коду">
</div>

## Во что можно играть

Игра запущена у хоста. Гость видит экран, слышит звук и управляет своим персонажем с геймпада. Игра видит геймпад гостя как отдельный контроллер.

| | DuoLink | Parsec | Steam Remote Play Together |
| --- | --- | --- | --- |
| Вход в сессию | Код и подтверждение хоста, без аккаунта | Аккаунты Parsec | Хост запускает игру в Steam и отправляет приглашение |
| Игры | Локальный мультиплеер на ПК хоста, включая файтинги и эмуляторы с поддержкой контроллеров | Приложения на ПК хоста | Игры с поддержкой Remote Play Together |
| Соединение | Встроенный прямой туннель по QUIC. Radmin VPN и ZeroTier не требуются | Подключение через Parsec | Подключение через Steam |
| Видео | AV1 или HEVC, если их поддерживают оба ПК. Битрейт можно задать вручную | HEVC при поддержке на обоих ПК | Настройки качества в Steam |

[Документация Parsec](https://support.parsec.app/hc/en-us/articles/32381199341716-Parsec-App-for-Windows) · [Регистрация в Parsec](https://support.parsec.app/hc/en-us/articles/32361325090836-Sign-Up) · [Steam Remote Play](https://store.steampowered.com/remoteplay)

## Первая игра

1. Скачайте и запустите `DuoLink.exe` на обоих ПК.
2. Запустите игру у хоста и нажмите «Создать сессию».
3. Отправьте другу код. Друг нажимает «Подключиться», хост нажимает «Разрешить». Добавьте второго игрока в самой игре.

> [!IMPORTANT]
> Для запуска нужен WebView2. DuoLink предложит [скачать его у Microsoft](https://developer.microsoft.com/microsoft-edge/webview2/#download), если компонент отсутствует. На первой сессии Windows может запросить установку ViGEmBus на ПК хоста. Он нужен для геймпадов гостей.

Ссылка в шапке ведёт прямо на установщик. ZIP со словом `portable` в имени подходит для запуска из распакованной папки.

> [!WARNING]
> EXE текущего релиза без цифровой подписи. Windows может показать предупреждение SmartScreen. Скачивайте файл из официального релиза DuoLink. Если Windows блокирует запуск, сохраните текст предупреждения и приложите его к отчёту. [Как Windows проверяет издателя](https://learn.microsoft.com/windows/apps/package-and-deploy/code-signing-options)

## FAQ

<details>
<summary>Другу нужна копия игры?</summary>

Достаточно копии у хоста. Гостю нужны DuoLink и геймпад, если он хочет играть за второго персонажа.

</details>

<details>
<summary>Можно подключиться без геймпада?</summary>

Да, в качестве зрителя. В играх на XInput доступно до четырёх контроллеров, включая контроллеры хоста. Если игра поддерживает смену геймпада на ходу, один гость может освободить место другому, оставаясь в трансляции.

</details>

<details>
<summary>Что делать при строгом NAT у обоих?</summary>

Попробуйте поменять хоста или подключить один ПК к другой сети. При доступе к роутеру проверьте UPnP. Если прямой маршрут так и не открывается, сессия не запустится.

</details>

<details>
<summary>Как сохранить друга и приглашать без копирования кода?</summary>

Откройте «Друзья», сохраните ник и добавьте друга по нику с тегом или по коду друга из настроек. Когда он примет запрос, используйте «Позвать».

</details>

## Документация

[Архитектура и сеть](docs/ARCHITECTURE.md) · [Сборка и свой сервер](docs/SELF_HOSTING.md) · [Сообщить об ошибке](https://github.com/roudinn/duolink/issues/new?template=bug_report.yml)

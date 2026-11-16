**Русский** | [English](README.en.md)

<div align="center">
  <h1>DuoLink</h1>
  <p>Локальный кооператив и versus через интернет на Windows.</p>
  <p><strong>Вход по коду без аккаунтов. Без встроенных лимитов FPS и разрешения.</strong></p>
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

Играйте вдвоём по интернету в игры с локальным мультиплеером и поддержкой нескольких геймпадов. Подойдут игры с разделённым экраном, файтинги и эмуляторы с локальным режимом. DuoLink передаёт видео и звук с ПК хоста, а геймпад гостя работает как отдельный контроллер.

## Чем DuoLink отличается

<table>
  <tr>
    <td>Возможность</td>
    <td><strong>DuoLink</strong></td>
    <td>Parsec</td>
    <td>Steam Remote Play Together</td>
  </tr>
  <tr>
    <td>Игра без аккаунта у обоих</td>
    <td><strong>Да, по коду</strong></td>
    <td>Нет</td>
    <td>Нет, хосту нужен Steam</td>
  </tr>
  <tr>
    <td>Игры вне Steam и эмуляторы</td>
    <td><strong>Да</strong></td>
    <td>Да</td>
    <td>Игры с поддержкой Remote Play Together</td>
  </tr>
  <tr>
    <td>Лимиты FPS и разрешения</td>
    <td><strong>Без встроенных лимитов</strong></td>
    <td>Ограничены</td>
    <td>Ограничены</td>
  </tr>
  <tr>
    <td>Игра без VPN и ручного проброса портов</td>
    <td><strong>Да, с автоматическим резервом по HTTPS</strong></td>
    <td>Да, если проходит прямой UDP</td>
    <td>Да</td>
  </tr>
</table>

## Первая игра

1. Скачайте и запустите `DuoLink.exe` на обоих ПК.
2. Запустите игру у хоста и нажмите «Создать сессию».
3. Отправьте другу код. Друг нажимает «Подключиться», хост нажимает «Разрешить». Добавьте второго игрока в самой игре.

> [!IMPORTANT]
> Для запуска нужен WebView2. DuoLink предложит [скачать его у Microsoft](https://developer.microsoft.com/microsoft-edge/webview2/#download), если компонент отсутствует. На первой сессии Windows может запросить установку ViGEmBus на ПК хоста. Он нужен для геймпадов гостей.

Ссылка в шапке ведёт прямо на установщик. ZIP со словом `portable` в имени подходит для запуска из распакованной папки.

> [!WARNING]
> EXE релиза без цифровой подписи, поэтому Windows может показать предупреждение SmartScreen. Исходный код DuoLink открыт. Установите [инструменты сборки](docs/SELF_HOSTING.md#windows-клиент), затем выполните `cargo build --release --locked`.
>
> Сверьте результат `Get-FileHash .\DuoLink.exe -Algorithm SHA256` с файлом `SHA256SUMS.txt` в релизе. Если Defender поместил файл в карантин, проверьте название обнаружения в журнале защиты и [отправьте файл в Microsoft на проверку](https://www.microsoft.com/en-us/wdsi/filesubmission).

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

Обновите DuoLink на обоих ПК. Если прямое подключение не получается, DuoLink попробует пройти через ретранслятор. На резервном маршруте битрейт ограничен, а задержка зависит от пути до сервера. Для сообщества можно [запустить свой ретранслятор](docs/SELF_HOSTING.md#свой-ретранслятор).

</details>

<details>
<summary>Как сохранить друга и приглашать без копирования кода?</summary>

Откройте «Друзья», сохраните ник и добавьте друга по нику с тегом или по коду друга из настроек. Когда он примет запрос, используйте «Позвать».

</details>

## Документация

[Архитектура и сеть](docs/ARCHITECTURE.md) · [Конфиденциальность](docs/PRIVACY.md) · [Сообщить об ошибке](https://github.com/roudinn/duolink/issues/new?template=bug_report.yml)

## Поддержать DuoLink

[Разовый донат через CloudTips](https://pay.cloudtips.ru/p/2d792cfa) помогает оплачивать сервер и развивать DuoLink.

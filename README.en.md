[Русский](README.md) | **English**

<div align="center">
  <h1>DuoLink</h1>
  <p>Play local co-op and versus games online on Windows.</p>
  <p><strong>Join by code without an account. No built-in caps on FPS or resolution.</strong></p>
  <p>
    <img alt="Windows 10 and 11, x64" src="https://img.shields.io/badge/Windows-10%20%2F%2011%20x64-303036?style=flat-square">
    <a href="https://github.com/roudinn/duolink/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/roudinn/duolink?style=flat-square&amp;color=303036"></a>
    <img alt="Rust 1.88+" src="https://img.shields.io/badge/Rust-1.88%2B-303036?style=flat-square">
  </p>
  <h3><a href="https://github.com/roudinn/duolink/releases/latest/download/DuoLink.exe">Download DuoLink for Windows</a></h3>
  <p><a href="https://github.com/roudinn/duolink/releases/latest">Portable build and release notes</a></p>
  <img src="docs/preview.en.png" width="1000" alt="DuoLink lobby with a guest and the join-by-code form">
</div>

## What you can play

You can play games with local multiplayer and support for multiple controllers, including games from Steam, other stores, and emulators. The guest sees the host's game and plays with a separate controller.

## First session

1. Download and run `DuoLink.exe` on both PCs.
2. Start the game on the host and select **Create session**.
3. Send the code to your friend. They select **Connect**, and the host selects **Allow**. Add the second player in the game.

> [!IMPORTANT]
> DuoLink needs WebView2. If it is missing, DuoLink offers to open [Microsoft's download page](https://developer.microsoft.com/microsoft-edge/webview2/#download). On the first session Windows may prompt to install ViGEmBus on the host. This driver provides the guest's gamepad to the game.

The main download is the installer. Extract the ZIP with `portable` in its name to run DuoLink from its own folder.

> [!WARNING]
> The current release EXE is unsigned. Windows may show a SmartScreen warning. Download the file from DuoLink's official release. If Windows blocks it, include the warning text in a bug report. [How Windows verifies publishers](https://learn.microsoft.com/windows/apps/package-and-deploy/code-signing-options)

## FAQ

<details>
<summary>Does my friend need a copy of the game?</summary>

The host's copy is enough. The guest needs DuoLink and a gamepad to play as a second player.

</details>

<details>
<summary>Can I join without a gamepad?</summary>

Yes, as a spectator. XInput games can see up to four controllers, including the host's. If the game supports controller changes during play, one guest can free a slot for another while staying in the stream.

</details>

<details>
<summary>What if both networks have strict NAT?</summary>

Update DuoLink on both PCs. If a direct connection fails, DuoLink tries the relay. The fallback limits bitrate, and latency depends on the route to the server.

</details>

<details>
<summary>How do I invite a friend without copying a session code each time?</summary>

Open **Friends**, save a nickname and add your friend by their nickname and tag or the friend code in Settings. Once they accept, use **Invite**.

</details>

## Documentation

[Architecture and networking](docs/ARCHITECTURE.md) · [Privacy (Russian)](docs/PRIVACY.md) · [Report a bug](https://github.com/roudinn/duolink/issues/new?template=bug_report.yml)

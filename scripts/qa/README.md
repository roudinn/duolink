# Windows latency and controller fixtures

`pad-to-pixel.cpp` feeds a virtual DS4 into Moonlight, observes Sunshine's XInput
controller, paints a GDI marker, then reads the corresponding pixel of the
Moonlight window with DXGI Desktop Duplication. It also paints a continuously
changing color marker for 12 seconds and records observed frame intervals.

This is **one-PC software observation**, not a physical button-to-photon test.
The source is a test marker, not a game. Window composition and the polling
interval are included; panel scanout and physical controller latency are not.
The source marker requests 144 Hz but Windows/GDI can deliver fewer updates.
The stream is explicitly configured separately (1080p, 144 FPS, 30 Mbps in the
recorded comparison). The first second of continuous marker frames is warm-up.

`controllers.cpp <Moonlight PID>` checks two local XInput controllers plus two
DS4 controllers forwarded through Moonlight, independent button states, removal
and replacement without ending the stream, and release of forwarded controllers
when the specified test Moonlight process closes. It requires all four XInput
slots initially free. Use only the PID of the test stream.

Both fixtures require ViGEmBus and the upstream ViGEmClient include/source files:
https://github.com/nefarius/ViGEmClient . Build in an x64 MSVC developer shell:

```bat
cl /nologo /O2 /EHsc /std:c++17 /DNOMINMAX /DUNICODE /D_UNICODE /I"ViGEmClient\include" pad-to-pixel.cpp ViGEmClient\src\ViGEmClient.cpp /link user32.lib gdi32.lib setupapi.lib xinput.lib winmm.lib d3d11.lib dxgi.lib
cl /nologo /O2 /EHsc /std:c++17 /DNOMINMAX /DUNICODE /D_UNICODE /I"ViGEmClient\include" controllers.cpp ViGEmClient\src\ViGEmClient.cpp /link user32.lib setupapi.lib xinput.lib
```

Before starting the Rust test, set `STREAM_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT`
to `0x054c/0x05c4` so the forwarded XInput devices cannot feed themselves back.
Set `DUOLINK_DATA_DIR` to an isolated folder, `DUOLINK_TEST_FPS=144` and
`DUOLINK_TEST_HOLD=35`. Run `cargo test --release --locked live_loopback --
--ignored --nocapture --test-threads=1`, then start one fixture when Moonlight's
streaming window appears. Capture stdout to a file. Do not compile or benchmark
other work while taking latency samples.

For five real simultaneous viewer streams, unset `DUOLINK_TEST_HOLD`, set
`DUOLINK_TEST_SPECTATORS=1` and `DUOLINK_TEST_FPS=60`, and run `live_loopback`.
The test joins five streams, removes spectators, leaves one player connected,
closes the host and creates a new lobby. GPU encoder capacity is required.

The network-only benchmark is `cargo test --release --locked
constrained_shared_upload -- --ignored --nocapture --test-threads=1`. It uses
real QUIC connections through one emulated 12 Mbps upload, a 250 ms queue and
5 ms propagation. It compares the old and new allocation policies with synthetic
frame bursts, with zero loss and deterministic 1% packet loss. It does not run
Sunshine's FEC or a video decoder and must not be called input-to-photon latency.

Run hardware/network tests separately to avoid fixed-port collisions:
`seven_guests_keep_independent_tunnels`, `live_audio_without_voice_chat`,
`process_loopback_activates`, `tunnel_round_trip_overhead`, `tunnel_throughput`.
Use `--ignored --nocapture --test-threads=1` for each filter.

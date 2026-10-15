# Hop

Low-latency remote desktop for two computers on the same network. Either computer can host or view: run Hop on both, click the other one, and you're controlling it. No accounts, no relay servers. Everything goes straight across your LAN, encrypted.

## Use it

**Mac:** `/Applications/Hop.app`. The window shows this Mac's **access code** and lists the other computers it can see.

**Windows**, any of these:

- **One line in PowerShell:**

  ```powershell
  iwr https://github.com/linpatr/hop/releases/latest/download/install.ps1 | iex
  ```

- **Download:** get `Hop.exe` from the latest [release](https://github.com/linpatr/hop/releases/latest), run it (SmartScreen: "More info" → "Run anyway", since the exe isn't code-signed) and click **Install Hop**.
- **From a Mac running Hop** (no internet needed): run `iwr http://<mac-ip>:47123/install.ps1 | iex`. The exact command, with the Mac's IP filled in, is shown in the Mac's Hop window.

All three install to `%LOCALAPPDATA%\Programs\Hop`, add a Start menu shortcut and a sign-in entry, and add a firewall rule (one admin prompt).

Then, on either computer, click **Connect** next to the other one. The first time, type the access code shown on the other computer. It's remembered after that.

- The Hop window shows who's connected to this computer, with a **Disconnect** button. The window flashes when someone connects.
- **Rename** sets the name your other computers see.
- Closing the Hop window keeps hosting in the background (minimized). **Quit Hop** at the bottom really quits.
- If Wi-Fi drops for a moment, the viewer reconnects by itself (for up to 30 seconds).
- The mouse pointer is your own computer's, so it never lags. It takes the remote pointer's shape (arrow, text cursor, resize arrows). With the mouse locked for games (M), the remote computer draws its pointer into the video instead.
- Five wrong access codes lock the host for a minute, and the viewer says so.

### Shortcuts while connected (Mac: Ctrl+Option+Shift)

| Ctrl+Alt+Shift + | |
|---|---|
| Q | disconnect |
| F | fullscreen on/off |
| M | lock the mouse to the window (games) |
| S | show/hide stats in the title bar |

When you connect, a hint with these shortcuts appears at the top of the viewer. **S** shows a stats panel: resolution, fps, bitrate, the latency breakdown (host + network + decode), and lost/repaired packets.

With "Send Cmd+Tab / Win key / Alt+Tab" on (default), system shortcuts on a Mac and Win / Alt+Tab / Alt+F4 on a PC go to the remote computer.

### macOS permissions

- **Screen & System Audio Recording**: to share this Mac's screen and sound.
- **Accessibility**: so the other computer can move the mouse and type, and so the viewer can capture Cmd+Tab.
- **Local Network**: macOS asks the first time Hop looks for the PC.

## How it works

```
host:   capture ──▶ H.264 hardware encode ──▶ FEC + encrypt ──▶ UDP ──┐
viewer: window ◀── GPU YUV→RGB ◀── hardware decode ◀── reassemble ◀────┘
        keyboard/mouse ──▶ encrypted TCP ──▶ host injects input
```

| | macOS | Windows |
|---|---|---|
| Capture | ScreenCaptureKit (screen + system audio) | DXGI Desktop Duplication + WASAPI loopback |
| Encode | VideoToolbox (hardware) | Media Foundation: NVIDIA/AMD/Intel hardware encoder, software fallback |
| Decode | VideoToolbox | Media Foundation (low-latency mode) |
| Render | wgpu → Metal | wgpu → D3D12/Vulkan |
| Input | CGEvent | SendInput (scancodes, so games see real keys) |

- **Latency choices:** no B-frames; keyframes only on demand (when packets are lost); no jitter buffer for video; frames are drawn the moment they decode (no vsync by default); the encoder always takes the newest frame.
- **Packet loss:** each frame is split into ~1.3 KB shards plus Reed-Solomon parity shards (15% by default; keyframes get more), so lost Wi-Fi packets are rebuilt without a stall. If a frame can't be rebuilt, the viewer asks for a keyframe.
- **Security:** the access code is never sent. Both sides run SPAKE2 with it, so a wrong code just produces different keys. A watcher on the network can't brute-force the code offline. All traffic is ChaCha20-Poly1305 encrypted. Five wrong codes lock the host for a minute.
- **Ports:** TCP 47121 (control/input), UDP 47122 (video/audio), UDP 47120 (discovery), TCP 47123 (Windows installer, Mac only).

## Build

```bash
./scripts/build-mac.sh          # dist/Hop.app (also bundles the Windows exe if built)
cargo build --release --target x86_64-pc-windows-gnu   # Windows exe, cross-compiled
cargo test
```

Needs Rust (rustup), Xcode, and `brew install mingw-w64` for the Windows build. Build output goes to `/Volumes/9100PRO/Developer/Cargo/hop-target` (see `.cargo/config.toml`).

Code map: `src/platform/` (mac.rs + `mac/HopMac.swift`, win/), `host.rs`, `client.rs`, `media.rs` (packets + FEC), `crypto.rs`, `window.rs` (viewer), `launcher.rs` (main window), `convert.rs` (BGRA→NV12 for Windows).

## Troubleshooting

- `hop selftest` (Windows: `& "$env:LOCALAPPDATA\Programs\Hop\Hop.exe" selftest`) captures 3 seconds, encodes, decodes, and prints timings and the encoder used.
- Logs: macOS `~/Library/Application Support/Hop/`, Windows `%APPDATA%\Hop\` (`hop.log`, `hop-viewer.log`, `hop-host.log`).
- `HOP_LOG=debug` for more detail. `HOP_ENCODER=nvidia` (or `amd`, `intel`, `microsoft`) forces a Windows encoder.
- Not listed? Connect by IP from the Hop window. Check the Windows firewall rule named "Hop".
- Wi-Fi works, but Ethernet on at least one side is the single biggest latency/smoothness improvement.

## Limits (for now)

- Streams the main display only.
- On Windows it can't see or control the UAC prompt, the lock screen, or windows of apps running as administrator (that needs a system service).
- One viewer at a time; a new connection replaces the old one.
- No gamepad forwarding.

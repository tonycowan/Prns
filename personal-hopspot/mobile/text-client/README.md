# Personal Text — Hopspot LocalClient

Minimal Dioxus app that **joins** Hopspot’s shared Reticulum instance and
announces `lxmf.delivery`. It never hosts.

## Prerequisites

1. Install and start **Personal Hopspot** on the phone (or `prnsd` on desktop
   with the local shared bus on `127.0.0.1:37428`).
2. Leave Hopspot running; this app attaches as a TCP LocalClient only.

## Message

1. **Announce** (so peers can path to you).
2. Tap a **heard** `lxmf.delivery` peer (e.g. Sideband).
3. Type a short text and **Send** (opportunistic LXMF single packet).

Inbound LXMF opportunistic packets show up under **Conversation**. Keep texts short (~240 chars) so they fit one encrypted packet.

## Desktop (Mac) — live LocalClient

```bash
cd personal-hopspot/mobile/text-client
dx serve --platform desktop
# or:
cargo run
```

With Hopspot/prnsd listening on `:37428`, status should flip to
**Connected (LocalClient)**. Press **Announce**.

Identity is stored at `~/.personal-text-client/lxmf_identity`.

Live bus smoke (optional):

```bash
PERSONAL_TEXT_LIVE_BUS=1 cargo test --test connect_smoke
```

## Web preview (mock only)

```bash
dx serve --platform web --features web --no-default-features
```

UI only — no RNS.

## Android

Requires Dioxus CLI **0.7.5** and the same NDK env as Hopspot:

```bash
export ANDROID_HOME="/opt/homebrew/share/android-commandlinetools"
export ANDROID_SDK_ROOT="$ANDROID_HOME"
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/27.2.12479018"
export NDK_HOME="$ANDROID_NDK_HOME"

cd personal-hopspot/mobile/text-client
dx bundle --platform android --features mobile --no-default-features
```

Install the debug APK (path under `target/dx/.../apk/debug/app-debug.apk`, also
copied to `dist/personal-text-client-debug.apk` when built here):

```bash
adb install -r dist/personal-text-client-debug.apk
```

Package id: `org.personal.textclient`. Start Hopspot first, then this app.

## What it does

| Action | Behavior |
|---|---|
| Startup | `connect_existing_shared_instance` → TCP `127.0.0.1:37428` |
| Hopspot down | Status **Waiting for Hopspot**; retries every 2s |
| Announce | `announce_now` on registered `lxmf.delivery` |
| Heard | Lists `AnnounceHeard` diagnostics from the LocalClient path |

## Out of scope (for now)

- LXMF message codec / Sideband chat interop
- Hosting a shared instance
- Binding Hopspot’s signature-protected `Messenger` API (TCP bus is enough)

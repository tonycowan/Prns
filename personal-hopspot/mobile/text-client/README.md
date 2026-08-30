# Personal Text — Hopspot LocalClient

Minimal Dioxus app that **joins** Hopspot’s shared Reticulum instance and
announces `lxmf.delivery`. It never hosts.

## Prerequisites

1. Install and start **Personal Hopspot** on the phone (or `prnsd` on desktop
   with the local shared bus on `127.0.0.1:37428`).
2. Leave Hopspot running; this app attaches as a TCP LocalClient only.

## Using the app

Three tabs:

| Tab | Purpose |
|---|---|
| **Me** | Connection status, your LXMF destination hash, announce count, **Announce** button |
| **Others** | Heard peers and message-only peers; tap a row to open **Chats** |
| **Chats** | Thread with the selected peer; compose and **Send** at the bottom |

Typical flow:

1. Open **Me** and tap **Announce** (so peers can path to you).
2. Open **Others** and pick a peer (heard announce, or a dashed placeholder row if they messaged you but have not announced yet).
3. In **Chats**, send short texts (~240 characters) via opportunistic LXMF.

### Range check

In-band GPS range test between two Personal Text clients:

1. Type exactly `Range check` (any capitalization) and send. The sender attaches GPS before the message goes out as `Range check (lat, lon)`.
2. The receiver sees an Accept/Deny prompt. Accept shares their GPS and replies with one message: `(lat, lon) - <distance>` (direct-line haversine). Deny sends nothing.
3. Only the receiving client calculates distance.

### Auto range check

Continuous 10-second cycle until either side stops it:

1. Type exactly `Auto range check` (any capitalization) and send. GPS is attached as `Auto range check (lat, lon)`.
2. The peer Accepts once (same location share consent as a one-shot check).
3. The **initiator** pings every **10 seconds** with a normal `Range check`; the peer auto-replies (no further prompts).
4. Either side ends the session by sending exactly `stop` (any capitalization).

Requires OS location permission (declared in `Dioxus.toml`). Desktop uses CoreLocation; Android uses the WebView geolocation API.

**Others tab details**

- Paste a **32-character destination hash** in the row at the top and tap **Find** to path-probe a peer (like `prnsd probe`). A successful probe adds them to the list with hop count and round-trip time.
- Each peer gets a default alias (`Alias 1`, `Alias 2`, …). Tap the alias to rename it.
- Tap anywhere else on the row to open that peer in **Chats**.
- The tab badge clears when you visit **Others**; unread message dots clear when you open that peer’s chat.
- Peers who sent a message without announcing appear with a dashed border and a warning that return path may be unavailable.

**Reading a heard-peer row**

Under the alias and destination hash, each announced peer shows a meta line like:

```text
Aug 22, 9:52:03 AM · hops 1 · local-client · aabbccddeeff00
```

Read it left to right:

| Part | Meaning |
|---|---|
| `Aug 22, 9:52:03 AM` | Local time when **you** last heard an announce from this peer (includes seconds). Updates when they re-announce. |
| `hops 1` | How many hops the announce traveled through the mesh before it reached your node. Lower is closer; `0` is direct/local. |
| `local-client` | Which **local** PRNS interface delivered the announce to Personal Text. For this app that is usually `local-client` (the Hopspot shared-instance TCP bus). Other values you might see if Hopspot forwards from wider mesh paths include `auto-wifi`, `bluetooth-peer`, `tcp-client`, and so on. |
| `aabbccddeeff00` | A 14-character hex fingerprint of that specific interface **instance** (PRNS channel-tag hash). Two rows with the same kind but different hex values are different physical/logical channels. |

Optional suffixes on the same line:

- `· selected` — this peer is the active **Chats** thread.
- `· new` — unread messages from this peer.

**Chats tab details**

- Header shows the peer alias (left) and **You** (right), with `…abcd` address tails underneath.
- Message bubbles show a local timestamp with seconds (e.g. `Aug 22, 9:52:03 AM`); delivery errors appear in bold red.
- New messages stay in view when you are already scrolled to the bottom.

## Desktop (Mac) — live LocalClient

```bash
cd personal-hopspot/mobile/text-client
dx serve --platform desktop
# or:
cargo run
```

With Hopspot/prnsd listening on `:37428`, status should flip to
**Connected (LocalClient)**.

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
dx bundle --platform android --features mobile --no-default-features --release --package-types apk --out-dir dist
```

Install the built APK (dx currently copies `app-debug.apk` into `dist/` even for `--release` Rust builds):

```bash
adb install -r dist/app-debug.apk
```

A previously kept release artifact may also appear as `dist/personal-text-client-release.apk` (~10 MB). Either installs package id `org.personal.textclient`. Start Hopspot first, then this app.

For a fully unstripped debug build (much larger APK), omit `--release`.

## Local data

| Data | Desktop | Android |
|---|---|---|
| LXMF identity | `~/.personal-text-client/lxmf_identity` | app files dir (`lxmf_identity`) |
| Peer aliases | `~/.personal-text-client/aliases.json` | app files dir (`aliases.json`) |

Identity is created on first launch and reused on later runs (stable destination hash).

## What it does

| Action | Behavior |
|---|---|
| Startup | `connect_existing_shared_instance` → TCP `127.0.0.1:37428` |
| Hopspot down | Status **Waiting for Hopspot**; retries every 2s |
| Announce | `announce_now` on registered `lxmf.delivery` |
| Heard | Lists `AnnounceHeard` diagnostics; Others rows show last-heard time, hop count, and a compact source-interface label (`kind · channel-hash`) |
| Send / receive | Opportunistic and direct LXMF (Sideband-compatible short texts) |
| Range check | Sender attaches GPS; receiver Accept replies with `(lat, lon) - distance` |
| Auto range check | Initiator pings 10s after each completed send; peer auto-replies until either side sends `stop` |

## Out of scope (for now)

- Hosting a shared instance
- Binding Hopspot’s signature-protected `Messenger` API (TCP bus is enough)

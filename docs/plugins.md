---
title: Plugins
description: Local plugin installation and API v1.
---

# Plugins (API v1)

ZapFast Plugins supports local `.zapplugin` ZIP packages without a store or
central server. In Settings, the Plugins section installs a package once and
allows it to be enabled or disabled independently for each account.
For incoming message plugins, account enablement is not enough: opt each
individual chat in from its conversation's More menu under "Plugins in this
chat". Only direct one-to-one chats are eligible. This explicit per-chat
selection grants the enabled plugin access to incoming text and up to 32
textual history messages from the previous seven days. Locked chats are not
eligible. Installation does not require rebuilding ZapFast.
Unlinking an account clears its plugin enablement and cancels in-flight work.

On Windows, ZapFast Plugins uses its own application and instance identity
and separate configuration, account and cache folders from ZapFast. It does
not import the original application's data; link an account separately.

**Only install plugins you trust.** A native plugin runs as a separate local
process, but it is not sandboxed: it retains the operating system permissions
of the user running ZapFast, including access to files and the network.
WhatsApp's terms and account restrictions may apply to automation.

## Package

The ZIP contains `manifest.json` and its executable files:

```json
{
  "id": "org.example.reply",
  "version": "1.0.0",
  "api_version": 1,
  "executables": {
    "linux-x86_64": "bin/reply",
    "windows-x86_64": "bin/reply.exe",
    "macos-aarch64": "bin/reply-mac"
  }
}
```

IDs contain lowercase ASCII letters, digits, hyphens and dot-separated
components. Versions use Semantic Versioning. The executable map keys use
Rust's OS and architecture names.
Paths are relative, use `/`, and may not contain `..`. Packages are limited
to 64 MiB and 32 regular files. Existing IDs are not overwritten. The host
extracts only the executable for its own platform.

## JSON-RPC

The host launches a fresh process for each event, writing newline-delimited
UTF-8 JSON-RPC 2.0 to stdin and reading it from stdout. Stdout is reserved
for JSON-RPC. The notification contains account, chat and message identifiers,
plus authorized incoming text and bounded local history:

```json
{"jsonrpc":"2.0","method":"message.received","params":{"account_id":"1","chat_id":"123@s.whatsapp.net","message_id":"ABC","text":"Hello","history":[{"from_me":true,"timestamp":1700000000,"text":"Hi"}]}}
```

History is chronological, local-only, limited to 32 messages from the prior
seven days, with each history text capped at 2 KiB and the incoming text at
16 KiB. Only text and captions are exposed; media bytes, protocol payloads,
keys and database access are not provided. History may be incomplete if the
phone has not supplied it. The event is for a new incoming direct message,
including one received while another account or conversation is displayed.
Own messages, history sync, recovery and duplicates are excluded. The plugin
may request a draft for the same event:

```json
{"jsonrpc":"2.0","id":1,"method":"draft.set_if_empty","params":{"account_id":"1","chat_id":"123@s.whatsapp.net","message_id":"ABC","text":"your draft"}}
```

The response is `{"jsonrpc":"2.0","id":1,"result":{"applied":true}}` or
`applied:false`. The host checks the event identifiers and limits text to
16 KiB. It checks both stored and unsaved text and attachments when applying
the request. A draft is never a send operation; this API exposes no message
send method. Invalid requests receive standard JSON-RPC errors (`-32700`,
`-32600`, `-32601` or `-32602`). A process gets three seconds and at most
64 KiB of output.
When busy, the host may drop events rather than block the chat.

## Example

The source is `plugins/example-reply`. It demonstrates access to incoming text
and history, then requests a deterministic draft describing what it received
and how many prior messages are available. It does not send. Build it
independently:

```sh
cargo build --release --manifest-path plugins/example-reply/Cargo.toml
python3 plugins/example-reply/package.py \
  --platform linux-x86_64 \
  --executable plugins/example-reply/target/release/zapfast-example-reply \
  --output example-linux-x86_64.zapplugin
```

Select the resulting file under Settings → Plugins, then enable it for the
account. To build on Windows x86-64 with the MSVC toolchain:

```powershell
cargo build --release --target x86_64-pc-windows-msvc --manifest-path plugins/example-reply/Cargo.toml
python plugins/example-reply/package.py --platform windows-x86_64 --executable plugins/example-reply/target/x86_64-pc-windows-msvc/release/zapfast-example-reply.exe --output example-windows-x86_64.zapplugin
```

On Apple Silicon macOS:

```sh
cargo build --release --target aarch64-apple-darwin --manifest-path plugins/example-reply/Cargo.toml
python3 plugins/example-reply/package.py --platform macos-aarch64 --executable plugins/example-reply/target/aarch64-apple-darwin/release/zapfast-example-reply --output example-macos-aarch64.zapplugin
```

Install the corresponding Rust target with `rustup target add` if necessary.
Cross-compilation also needs an appropriate linker and platform SDK. Build
native executables for each architecture you distribute.

## Verification coverage

The example package and automated, synthetic-data integration tests have
been exercised on Ubuntu Linux x86-64 with glibc 2.39. The included example
binary may require a compatible glibc; rebuild it for older systems.
The Windows x86-64 example was installed and run by a user on native Windows
with preview 3, after correcting an access-denied error during installation.
An automated Windows-target installation test also passed under Wine. These
checks do not establish full feature coverage. macOS builds and live WhatsApp
delivery remain to be verified; the Linux tests do not require a real account.

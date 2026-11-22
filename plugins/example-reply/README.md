# Example reply plugin

This standalone program is an example for the ZapFast Plugins v1 API.
See [the plugin guide](../../docs/plugins.md) for installation and the
JSON-RPC protocol.

`example-linux-x86_64.zapplugin` contains the Linux x86-64 executable and
manifest.

`example-windows-x86_64.zapplugin` contains a separately cross-compiled
Windows x86-64 executable. It has not been tested on a native Windows system.

Build it independently with:

```sh
cargo build --release --manifest-path plugins/example-reply/Cargo.toml
```

It reads newline-delimited JSON-RPC messages on stdin. For a
`message.received` notification with nonempty `account_id`, `chat_id` and
`message_id` strings, it writes a `draft.set_if_empty` request with the same
identifiers and the text `en breve te responderemos`. It never requests
message sending.

Native plugins execute with the operating system permissions of the user.
JSON-RPC does not sandbox them. Install only plugins whose authors and
executables you trust. Automation may also be subject to WhatsApp's terms
and account restrictions.

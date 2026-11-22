# Example reply plugin (work in progress)

This standalone program is an example for the planned plugin API. ZapFast
Plugins does not yet include a plugin host or installer, so this example
cannot currently be installed or used in the application. The protocol and
manifest are provisional.

`example-linux-x86_64.zapplugin` is a provisional ZIP containing the
Linux x86-64 executable and manifest. It is not yet usable by the application.

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

# Access links

To keep bt private, give each person a UUID (`uuidgen`) and list them in the [config file](configuration.md), with the address listeners open:

```toml
public_url = "https://bt.example.com"

[access]
lorena = "<uuid>"
luis = "<uuid>"
```

(Or set `BT_ACCESS_TOKENS=lorena:<uuid>,luis:<uuid>` and `BT_PUBLIC_URL`.) Each person opens their access link, `<public_url>/?token=<uuid>`, once; their browser then stays in for a year. Everyone else gets a 401. To revoke a link, remove its entry and restart bt.

bt logs every access link on startup, so don't share its logs (`make logs`) publicly.

## Behind a reverse proxy

`public_url` must be exactly the origin listeners type (scheme, host and port), because `/ws` only accepts pages loaded from it. If the page loads but never connects, the WebSocket is getting a 403: check that `public_url` matches the address bar. Locally, open `http://localhost:3000`, not `http://127.0.0.1:3000`, if that's what `public_url` says.

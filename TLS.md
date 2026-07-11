# TLS operation

TokenWeasel terminates inbound TLS for the dashboard, management endpoints, and proxied LLM requests on its single listener. TLS for connections from TokenWeasel to a backend is separate and follows each backend's `http://` or `https://` upstream URL.

## Defaults

TLS is enabled by default. With no TLS configuration, TokenWeasel listens with HTTPS and uses these values:

| Setting | Default |
|---|---|
| `enabled` | `true` |
| `cert_path` | `tls/cert.pem` |
| `key_path` | `tls/key.pem` |
| `hostnames` | `["localhost"]` |

On the first start, if the certificate or key is missing, TokenWeasel creates the parent directories, generates a self-signed certificate and private key, writes both files, and loads them. On later starts, it reuses the pair when both files exist, preserving the certificate fingerprint. If either file is missing, TokenWeasel generates and writes a new pair; keep the two files together when backing up or replacing them.

## Configuration and overrides

Configure TLS in `config.json`:

```json
{
  "tls": {
    "enabled": true,
    "cert_path": "tls/cert.pem",
    "key_path": "tls/key.pem",
    "hostnames": ["localhost", "127.0.0.1", "proxy.internal.example"]
  }
}
```

The usual precedence applies: command-line option, then environment variable, then configuration file, then built-in default.

| Purpose | Command line | Environment |
|---|---|---|
| Enable or disable TLS | `--tls true` / `--tls false` | `TLS_ENABLED=true` / `TLS_ENABLED=false` |
| Certificate path | `--tls-cert PATH` | `TLS_CERT_PATH=PATH` |
| Private-key path | `--tls-key PATH` | `TLS_KEY_PATH=PATH` |

`hostnames` is configured only in the JSON TLS block and is used only when TokenWeasel generates a certificate.

## Certificate names and trust

Every address clients use must appear in the generated certificate's subject alternative names (SANs). DNS names and IP addresses are distinct: a certificate generated only for `localhost` does not validate a connection to `127.0.0.1`. Add each actual DNS name or IP address to `hostnames` before generation. To change the SANs later, move or remove both generated files and restart so TokenWeasel creates a new pair.

A self-signed certificate is not trusted automatically. Import `tls/cert.pem` into the client's trust store or configure the client to use it as a CA. For example:

```sh
curl --cacert tls/cert.pem https://localhost:3000/health
```

Avoid disabling certificate verification except for short-lived local diagnostics.

## Supplying a certificate

For production, place a PEM certificate chain and its matching PEM private key at the configured paths before starting TokenWeasel. When both files exist, TokenWeasel loads them without generating replacements. The certificate must cover the client-facing names; the `hostnames` setting does not modify a supplied certificate.

The process user needs read access to both files. Restrict the private key to that user: generated keys are created with mode `0600` on Unix, while supplied keys retain their existing permissions. On non-Unix systems, use the platform's filesystem ACLs. Never commit private keys or include them in build or distribution artifacts.

## TLS termination at an ingress

Disable TokenWeasel TLS only when a trusted reverse proxy, load balancer, or ingress terminates HTTPS in front of it:

```sh
TLS_ENABLED=false twl
```

or:

```json
{
  "tls": {
    "enabled": false
  }
}
```

Protect the plaintext hop between the terminator and TokenWeasel with a private network or equivalent transport security. API-key and dashboard authentication defaults remain enabled independently of TLS.

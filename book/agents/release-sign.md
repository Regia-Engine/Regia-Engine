# Release sign and upload

`regia release sign` writes a detached Ed25519 signature next to each release manifest.
`regia release upload` copies that directory through a filesystem backend (ADR-8687) or
uploads it through a hosting CLI named in a config file (ADR-8846).
`regia release verify` checks a signature when one is present.

```bash
regia release sign <dir> --key <path> --date YYYY-MM-DD [--project DIR] [--json]
regia release verify <dir> [--require-signature] [--trust-key KEY] [--json]
regia release upload <dir> --backend fs:<dir> [--dry-run] [--retries N] [--json]
regia release upload <dir> --backend cli:<config.toml> [--var KEY=VALUE]... [--dry-run] [--retries N] [--json]
```

`sign` uses the same key file as `regia assets keygen`: the first line is
`regia-content-signing-key v1` and the second line is PKCS#8 hex. The command does not
generate a key and it does not read a clock. `--date` is `YYYY-MM-DD`. A key whose path is
inside the project is `content_key_in_project`, and the key file is not read.

The signed payload is the manifest file name, the SHA-256 of the manifest bytes, and the
date, prefixed by a release-specific domain so the signature cannot be replayed as a content
manifest signature. The detached file is `<manifest>.sig` (for example
`manifest-demo.json.sig`). Its JSON records `format` 1, `algorithm` `ed25519`, `key_id`,
`public_key`, `date`, `manifest_sha256` and `signature`.

`verify` checks a `.sig` that is present. `--require-signature` also fails when the `.sig`
is absent. `--trust-key` may be repeated. When it is set, the public key in the signature
must be one of those keys. When it is omitted, the embedded public key is enough to prove
the manifest bytes and the date were not modified.

`upload` follows the upload plan: chunks, then other payload, then manifests. A `.sig` is
payload under that classification, so it is uploaded before the manifests.

`--backend fs:<directory>` copies the files into a local directory. `publish` writes
`PUBLISHED` containing `OK`. `--dry-run` prints the names and does not write them.

`--backend cli:<config.toml>` runs the release host's own upload program once per file, as a
child process with no shell (ADR-8846). The config is TOML:

```toml
upload_cli = 1                       # version of this config; nothing else is read
program = "hostcli"                  # a name on PATH, or a path; no placeholders
put = ["push", "{file}", "{target}:{channel}/{name}"]   # must pass {file}
list = ["ls", "{target}:{channel}"]  # optional: prints `SIZE NAME` lines
publish = ["release", "{target}:{channel}"]             # optional: runs once at the end
secret_env = ["HOSTCLI_API_KEY"]     # credential variables, names only
timeout_s = 1800                     # per child process, 1 to 86400

[vars]
target = "studio/game"
channel = "beta"
```

Placeholders are `{file}` (the local path), `{name}` (its base name), `{dir}` (the release
directory) and each `vars` key; `--var channel=linux` overrides a key the config defines.
`list` and `publish` may not use `{file}` or `{name}`. Unknown keys and placeholders are
refused. When `list` is set, a remote file of the same size is skipped. A `put` that exits
non-zero is run again up to `--retries` attempts in total; `list` and `publish` run once. A
failed `put` stops the upload before `publish`.

Credentials never go in the config, the repository, the output or a report. The child reads
them from the environment variables `secret_env` names; each must be set and non-empty for a
real upload (`upload_cli_env`). A config whose program, arguments or vars contain the value of
one of those variables is refused before anything runs (`upload_cli_secret`; the message names
the variable and the field, never the value). Every value of 4 bytes or more is replaced by
`<redacted:NAME>` in every command line, output tail, JSON field and error this command
prints.

`--dry-run` with `cli:` starts nothing. It prints `would run <command line>` for each step in
order and `secret NAME set` or `secret NAME unset` for each credential variable; `--json` puts
the same in `commands` and `secret_env`. A real run prints `ran <command line> (status N,
attempt K)` per child, and `--json` adds `calls` with `argv`, `status`, `attempt` and the last
2 KiB of `stdout` and `stderr`, redacted.

The sidecar is the frozen format `release-signature` v1
([`docs/formats/release-signature.md`](../../docs/formats/release-signature.md); fuzzed by
`crates/regia_release_chunks/tests/fuzz.rs`, the format's row in the fuzz-target registry,
ADR-10356).

`--json` documents use schema 1, with `ok`, `command`, and on failure `code` and `message`.

Exit codes: verify exits 0 when the directory is clean and 1 when a check fails. Sign and
upload exit 0 after they finish and 2 when an input cannot be used.

## Agent method

`release.sign_status` and `release.upload_dry_run` are editor-host methods (ADR-10356): a host that
installs the editor's agent surface answers them (the editor and its headless agent host). They are
not in the runtime schema table, so an app that runs only the runtime agent answers method-not-found.

`release.sign_status` takes `dir` (required). Permission `Read`. It does not change the
world. A missing `dir` is `F0002`. An empty `dir`, or a `dir` that is not a directory, is
`release_dir`, and the working directory is not scanned in its place. A directory that can
be read answers success. `ok` is true only when every manifest has a signature that
verifies. An unsigned manifest answers success with `ok` false and a `manifest_signature`
problem.

`release.verify` also accepts `require_signature` (boolean) and `trust_keys` (array of
`ed25519:` public keys).

`release.upload_dry_run` takes `dir` and `config` (both required) and optional `vars` (an
object of strings overriding the config's `vars`). Permission `Read`; nothing is started or
written. It answers `{ ok, dir, files, commands, secret_env }`: the planned file names in
upload order, the hosting-CLI command lines in run order (program first, redacted) and
`[{ name, set }]` per credential variable. A credential value is never returned. Codes: `F0002`
(a missing or mistyped param), `release_dir`, `upload_cli_config`, `upload_cli_secret`,
`unsafe_path` and `io_error` (the config file cannot be read).

```json
{ "method": "release.upload_dry_run", "params": { "dir": "build/release", "config": "release-host.toml", "vars": { "channel": "linux" } } }
```

There is no agent method that signs or uploads. Use the CLI. Tests generate a throwaway key
in a temp directory. Do not commit a signing key.

## Codes

These names are `ChunkError` codes, or a verify `check`. Protocol codes from the agent are
`F0002` and `release_dir`.

| Code | When |
|---|---|
| `content_key_in_project` | The private key path is inside the project. The key is not read. |
| `release_date` | `--date` is not a calendar `YYYY-MM-DD` from 1970 onward. |
| `release_key` | The key file is missing or is not an Ed25519 PKCS#8 key of this header. |
| `release_manifest` | The directory has no manifest JSON to sign. |
| `release_dir` | The directory is missing. On the agent this is also the protocol code. |
| `manifest_signature` | The signature is missing under `--require-signature`, altered, signed by an untrusted key, or the manifest bytes no longer match. |
| `upload_backend` | `--backend` is not `fs:` plus a directory or `cli:` plus a config file. |
| `upload_cli_config` | The `cli:` config does not parse, has an unknown key or placeholder, another `upload_cli` version, a `put` without `{file}`, a bad `vars` or `secret_env` name, a `timeout_s` outside 1 to 86400, or a `--var` that is not `key=value` of a defined key. |
| `upload_cli_env` | A `secret_env` variable is unset or empty for a real upload. |
| `upload_cli_secret` | The config's program, arguments or vars hold the value of a `secret_env` variable. |
| `upload_cli_spawn` | The hosting CLI program could not be started. |
| `upload_cli_failed` | A child exited non-zero after its attempts, or printed more than 4 MiB. The message has the redacted end of its standard error. |
| `upload_cli_timeout` | A child ran longer than `timeout_s` and was stopped. It is not retried. |
| `upload_cli_list` | A `list` output line is not `SIZE NAME`. |
| `release_signature_newer` | A `.sig` has a `format` above 1 (the canonical reader and the fuzz target; `verify` reports `manifest_signature`). |
| `unsafe_path` | A planned upload name is not a single file name. |
| `io_error` | A path could not be read or written. |

# Hutch-lite live demo

`crates/rvbbit/rvbbit_hutch_lite` is an early hutch that speaks
`rvbbit-hutch-v1` and calls an OpenAI-compatible chat endpoint. The
binary is `hutch_lite`. It does not call a provider until
`OPENROUTER_API_KEY` is set, and it never prints that key.

This VM does not have a key. Run the demo on a machine that does.

## What the demo does

`scripts/rvbbit_live_demo.sh` builds pgrustic and hutch-lite, starts both
on ports that do not collide with the spike (Postgres `55433`, hutch
`8098`), creates a fresh database, and loads six bigfoot-style sightings.

It then runs:

- `notes MEANS 'a large hairy biped was seen'` (one batched `/v1/predict`)
- `rvbbit.creature(notes)` (one `/v1/extract` per row)

and runs both again. The second pass must not increase `provider_calls`.
`hits` must rise. Twelve provider calls is the expected first pass
(six predict items plus six extracts). The script aborts above forty.

`TAKES` / `PICK` are not in the demo. The parser still rejects them until
milestone M5. Embed, rerank, and decide are per-item `unsupported`.

## Fresh Ubuntu

```bash
sudo apt-get update
sudo apt-get install -y build-essential g++ pkg-config ca-certificates curl \
  postgresql-18 postgresql-client-18
```

`/usr/bin/c++` on this image is clang, and re2's `<algorithm>` include
fails under that driver. The script exports `CC=gcc` and `CXX=g++`.
Install `g++` (pulled in by `build-essential`) so those names exist.

The link also needs the unversioned libstdc++ name. If this file is
missing, create it as root and rerun the script. The script prints the
same command and does not run `sudo` itself:

```bash
sudo ln -s libstdc++.so.6 /usr/lib/x86_64-linux-gnu/libstdc++.so
```

Rust is `1.96.0`, pinned by `rust-toolchain.toml`. Install rustup, then
let the first `cargo` invocation download that toolchain:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Postgres share files must be the 18 package layout:

- `PGRUST_PGSHAREDIR=/usr/share/postgresql/18`
- `PGRUST_TZDIR=/usr/share/zoneinfo`

`initdb` and `psql` come from `/usr/lib/postgresql/18/bin`. The script
puts that directory on `PATH`.

## Run

```bash
export OPENROUTER_API_KEY=...    # never commit this; the script does not echo it
# optional:
# export HUTCH_MODEL=openai/gpt-4o-mini
# export OPENROUTER_BASE_URL=https://openrouter.ai/api/v1
# export HUTCH_TIMEOUT_MS=60000
bash scripts/rvbbit_live_demo.sh
```

Leave `BACKEND AUTH ENV` unset. That column is the bearer hutch-lite
expects from pgrustic. The OpenRouter key stays in hutch-lite's
environment. Putting the key in `AUTH ENV` would send it to hutch as a
bearer and would not authorize the upstream call.

`HUTCH_MODEL` defaults to `openai/gpt-4o-mini`. The script writes that
name into `CREATE MODEL ... NAME`. A request model of `mock` or empty
also falls back to `HUTCH_MODEL`. `OPENROUTER_BASE_URL` points the chat
call at `{base}/chat/completions`, which is how the crate tests use a
local fake.

Opt-in crate test, one predict, skipped unless you pass `--ignored` and
the key is set:

```bash
cargo test -p rvbbit_hutch_lite -- --ignored live_openrouter_means
```

Fake-endpoint tests (no key, no network provider):

```bash
cargo test -p rvbbit_hutch_lite
```

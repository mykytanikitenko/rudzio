# First publish to crates.io

The `release.yml` GitHub Actions workflow handles every release **after**
v0.1.0. The very first publish round (v0.1.0) has to run manually because
of a known cargo limitation:

> Every workspace member with a dev-dep on a sibling cannot run
> `cargo publish` (or even `--dry-run`) until that sibling already
> exists on crates.io. cargo strips path deps when packaging, then
> tries to resolve the version-only form from the registry, which
> doesn't have it yet.

Crates affected in this workspace:

| Crate | Dev-dep on workspace sibling |
|---|---|
| `rudzio-macro-internals` | `rudzio` |
| `rudzio` | `rudzio` (self) |
| `cargo-rudzio` | `rudzio` |

`cargo-rudzio`'s dev-dep is fine on first publish because by the time
its turn comes, `rudzio` is already on crates.io. So the only Cargo.toml
files needing temporary surgery are `macro-internals/Cargo.toml` (before
the leaf publish) and the root `Cargo.toml` (before rudzio's publish).

## Manual flow (one-shot for v0.1.0)

Run from a clean checkout on the `v0.1.0` tag:

```sh
# 1. Publish rudzio-macro-internals — needs its rudzio dev-dep stripped.
sed -i '/^rudzio = { workspace = true,/,/^] }$/d' macro-internals/Cargo.toml
cargo publish -p rudzio-macro-internals
git checkout -- macro-internals/Cargo.toml
sleep 30  # wait for crates.io index propagation

# 2. rudzio-macro: regular dep on macro-internals (now on crates.io). No surgery.
cargo publish -p rudzio-macro
sleep 30

# 3. rudzio: strip its self-dev-dep block before publish.
sed -i '/^rudzio = { version = "0.1.0", path = "\.",/,/^] }$/d' Cargo.toml
cargo publish -p rudzio
git checkout -- Cargo.toml
sleep 30

# 4. rudzio-migrate: regular dep on rudzio (now on crates.io). No surgery.
cargo publish -p rudzio-migrate
sleep 30

# 5. cargo-rudzio: regular dep on rudzio-migrate (now on crates.io),
#    dev-dep on rudzio (now on crates.io). No surgery.
cargo publish -p cargo-rudzio
```

`CARGO_REGISTRY_TOKEN` must be set in the environment running these
commands (the same token the CI runner has).

## After v0.1.0 lands

All five crates are now on crates.io. From v0.1.1 onward:

```sh
# Bump every crate's `version = "0.1.1"` (root + macro + macro-internals
# + migrate + cargo-rudzio Cargo.toml; all 5 stay locked together).
git tag v0.1.1
git push origin v0.1.1
```

The `release.yml` workflow takes it from there — no surgery needed
because dep resolution against crates.io succeeds for every crate.

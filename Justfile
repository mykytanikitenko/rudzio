# Host-side Justfile (entrypoint). Only recipes that run with the
# operator's system tools (git, gh) live here. Every recipe that
# depends on nix-devShell tooling (cargo, rustfmt, clippy, taplo,
# asciinema, etc.) is delegated to Justfile.nix via `import` below.
#
# Architecture rationale:
#   * `Justfile.nix` recipes 127 outside `nix develop` — keeping them
#     in a separate file makes the split explicit at file level
#     instead of per-recipe.
#   * Pre-PR-#1, flake.nix's shellHook carried `alias just='just -f
#     Justfile.nix'` to route everything through Justfile.nix. That
#     alias was REMOVED by accident in 66fa12b. The `import` directive
#     below is the proper reincarnation: idempotent, no shell alias
#     drift, works inside or outside `nix develop` for any recipe
#     defined in Justfile.nix.
#   * Inside nix: `just ci` resolves to Justfile.nix's `ci` recipe via
#     the import.
#   * Outside nix: `just nix` enters the shell; OR `nix develop
#     --command just ci` runs the nix recipe directly without an
#     interactive shell entry (CI invocation path).

set shell := ["bash", "-euo", "pipefail", "-c"]

# Pull in every recipe that needs the nix devShell toolchain. Recipes
# defined in Justfile.nix become invocable here directly (e.g.,
# `just ci`, `just check-fmt`, etc.).
import 'Justfile.nix'

# List available recipes (both host and imported nix recipes show up)
default:
    @just --list

# Enter nix development shell
nix:
    nix develop

# Activate the in-repo git hooks (.githooks/) for this checkout.
# Currently installs a commit-msg hook that rejects AI-attribution
# trailers (`Co-Authored-By: Claude`, etc.). Run once per clone.
setup-hooks:
    git config core.hooksPath .githooks
    @echo "Hooks active: .githooks/"

# --- CI/CD shortcuts (gh CLI, runs host-side) ---

# Recent CI runs
ci-status:
    gh run list --workflow=ci.yml --limit 10

# Trigger CI on the current branch
ci-trigger:
    gh workflow run ci.yml --ref "$(git rev-parse --abbrev-ref HEAD)"

# Watch the most recent CI run
ci-watch:
    gh run watch

# Recent release runs
release-status:
    gh run list --workflow=release.yml --limit 10

# Watch the most recent release run
release-watch:
    gh run watch

# Tag current commit and instruct on push, e.g. `just release-tag 0.2.0`
release-tag VERSION:
    git tag -a "v{{VERSION}}" -m "Release v{{VERSION}}"
    @echo "Created tag v{{VERSION}}. Push it to trigger crates.io publish:"
    @echo "    git push origin v{{VERSION}}"

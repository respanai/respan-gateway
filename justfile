# Run `just` to list the recipes.

ui := "apps/ui"

# List the recipes.
default:
    @just --list

# The gateway on :8080 and the UI with hot reload on http://localhost:5173/ui/. Ctrl-C stops both.
dev *args: ui-deps
    #!/usr/bin/env bash
    set -euo pipefail
    gateway_pid=
    ui_pid=
    trap 'kill "$gateway_pid" "$ui_pid" 2>/dev/null || true' EXIT
    cargo run -p respan-gateway -- {{args}} &
    gateway_pid=$!
    (cd {{ui}} && pnpm dev) &
    ui_pid=$!
    is_running() {
        for running_pid in $(jobs -pr); do
            [ "$running_pid" = "$1" ] && return 0
        done
        return 1
    }
    while is_running "$gateway_pid" && is_running "$ui_pid"; do
        sleep 1
    done
    status=0
    if is_running "$gateway_pid"; then
        wait "$ui_pid" || status=$?
    else
        wait "$gateway_pid" || status=$?
    fi
    exit "$status"

# The UI with hot reload against a fake gateway; no Rust needed.
mock: ui-deps
    #!/usr/bin/env bash
    set -euo pipefail
    mock_pid=
    ui_pid=
    trap 'kill "$mock_pid" "$ui_pid" 2>/dev/null || true' EXIT
    (cd {{ui}} && pnpm mock) &
    mock_pid=$!
    (cd {{ui}} && pnpm dev) &
    ui_pid=$!
    is_running() {
        for running_pid in $(jobs -pr); do
            [ "$running_pid" = "$1" ] && return 0
        done
        return 1
    }
    while is_running "$mock_pid" && is_running "$ui_pid"; do
        sleep 1
    done
    status=0
    if is_running "$mock_pid"; then
        wait "$ui_pid" || status=$?
    else
        wait "$mock_pid" || status=$?
    fi
    exit "$status"

# Build the UI, then run the gateway serving it at http://localhost:8080/ui/.
run *args: ui-build
    cargo run -p respan-gateway -- {{args}}

# Build the UI into apps/ui/dist, which the gateway embeds.
ui-build: ui-deps
    cd {{ui}} && pnpm build

# Install the UI's packages, if they are not yet.
ui-deps:
    @test -d {{ui}}/node_modules || (cd {{ui}} && pnpm install --frozen-lockfile)

# The checks CONTRIBUTING.md asks for: Rust formatting, lints and tests, and the UI's.
check: ui-deps
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-targets --all-features
    cargo test --workspace --doc --all-features
    cd {{ui}} && pnpm lint && pnpm typecheck && pnpm test && pnpm build

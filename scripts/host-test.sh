#!/usr/bin/env bash
# Run the unit tests for the hardware-independent modules (rapt, pills, fleet, settings, dns, pages) on the host.
# `cargo test` can't be used: this crate always builds for the xtensa-esp32-espidf target.
set -euo pipefail
src="$(cd "$(dirname "$0")/../src" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cat > "$work/host_tests.rs" <<EOT
#[path = "$src/rapt.rs"] #[allow(dead_code)] mod rapt;
#[path = "$src/pills.rs"] #[allow(dead_code)] mod pills;
#[path = "$src/fleet.rs"] #[allow(dead_code)] mod fleet;
#[path = "$src/settings.rs"] #[allow(dead_code)] mod settings;
#[path = "$src/dns.rs"] #[allow(dead_code)] mod dns;
#[path = "$src/pages.rs"] #[allow(dead_code)] mod pages;
EOT
rustc +stable --edition 2021 --test -o "$work/host_tests" "$work/host_tests.rs"
"$work/host_tests" "$@"

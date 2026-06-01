# Vendored Lux

This directory vendors the Lux library source from https://github.com/lux-db/lux at commit `32cf2b6e1e043be677d63f20dacae47c5a7ea468`.

Local adjustments:

- `lib.rs` was renamed to `mod.rs` so Lux compiles as an internal module.
- Internal `crate::` paths were rewritten to `crate::vendor::lux::`.
- Upstream internal test modules were disabled with `#[cfg(any())]` so MySqweel tests do not require Lux-only dev dependencies.
- Vendored lint noise is suppressed at the module boundary.

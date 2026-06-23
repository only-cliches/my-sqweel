# Vendored Lux

This directory vendors the Lux library source from https://github.com/lux-db/lux at commit `142a396db615b0adf949cf533a4aa6f65c93b2d2`.

Local adjustments:

- `lib.rs` was renamed to `mod.rs` so Lux compiles as an internal module.
- The standalone binary entry point `main.rs` is omitted.
- Internal `crate::` paths were rewritten to `crate::vendor::lux::`.
- Upstream internal test modules were disabled with `#[cfg(any())]` so MySqweel tests do not require Lux-only dev dependencies.
- Vendored lint noise is suppressed at the module boundary.

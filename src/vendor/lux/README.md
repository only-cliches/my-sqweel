# Vendored Lux

This directory vendors the Lux library source from https://github.com/lux-db/lux at commit `1fd111278c50e0f349eda37c3d9c7cf42dc948cc` (0.37.0).

Local adjustments:

- `lib.rs` was renamed to `mod.rs` so Lux compiles as an internal module.
- The standalone binary sources are retained for provenance but omitted from
  the internal module tree.
- Internal `crate::` paths were rewritten to `crate::vendor::lux::`.
- Upstream internal test modules are disabled with `#[cfg(any())]` where they would require Lux-only dev dependencies.
- Vendored lint noise is suppressed at the module boundary.

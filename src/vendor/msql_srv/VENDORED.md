# Embedded protocol server

Adapted from msql-srv 0.11.0 (https://github.com/jonhoo/msql-srv), licensed under MIT OR Apache-2.0; both upstream license files are retained here.

This module includes MySqweel's authentication, transaction-status, warning-count, and error-code extensions. It is compiled directly into MySqweel so Cargo packages use the same protocol implementation as working-tree builds. Imports are scoped to this module, and the original standalone example is documentation-only. TLS remains enabled by default, as in the upstream dependency.

The upstream unit tests remain alongside the implementation and run with the main crate's unit tests.

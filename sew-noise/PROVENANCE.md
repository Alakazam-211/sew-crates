# Provenance

What this crate's noise-removal model is and where it comes from.

| | |
|---|---|
| Inference code | `nnnoiseless` 0.5.2, from crates.io, pinned exactly (`=0.5.2`), default features off |
| Package checksum (crates.io, sha256 of the `.crate` file) | `805d5964d1e7a0006a7fdced7dae75084d66d18b35f1dfe81bd76929b1f8da0c` |
| Upstream repository | <https://github.com/jneem/nnnoiseless> |
| Licence | BSD-3-Clause (the package's `COPYING`, sha256 `26e8aae6fb3622e281d8d99aa7ba0df8ef3a83465d8717f2e4cacbcbed8efc92`; its text is in NOTICE) |
| Model weights | `src/weights.rnn` inside that package, compiled into `nnnoiseless` and so into this crate |
| Weights file sha256 | `e6de5fbfadf7ec91d1b24d6a6ccfd0290cb4d8bf555c5eab3ce41506f67a58b1` |
| Weights file size | 87521 bytes |

`nnnoiseless` is a port to Rust of RNNoise (<https://gitlab.xiph.org/xiph/rnnoise>), and its
weights file is the model that RNNoise shipped. This crate adds no weights of its own, does not
change the file and loads nothing at run time.

To check: download `nnnoiseless` 0.5.2 from crates.io, compare the package's sha256 with the
checksum above (it is also the `checksum` of the `nnnoiseless` entry in a `Cargo.lock`), and
run `sha256sum src/weights.rnn` in the unpacked package.

## DeepFilterNet3 (feature `dfn3`)

The `dfn3` feature runs the low-latency variant of DeepFilterNet3 (by Hendrik Schröter and the
DeepFilterNet authors; MIT OR Apache-2.0) with this crate's own inference code. Its weights are
not in this package; they are loaded at run time from a file the app ships, and only if that
file's sha256 is the one this version pins (`dfn3::WEIGHTS_SHA256`).

| | |
|---|---|
| Upstream repository | <https://github.com/Rikorose/DeepFilterNet> |
| Upstream file | `models/DeepFilterNet3_ll_onnx.tar.gz` at commit `978576aa8400552a4ce9730838c635aa30db5e61` (the v0.5.6 tag), sha256 `5998e58e8ba0e09bb76986ef97b84afa065a571ef282d4a1222f341e3251cf3a` |
| File loaded | `dfn3_ll.nnef.tar`, sha256 `b6e52c0bc66b82e42e3403d22cdd3b847143026d6a41cc4380a97b044f7766af`, 39,034,880 bytes: the upstream file's float32 weights, renamed and stored as NNEF tensor files (values unchanged), converted by Alakazam Labs |
| Converter | `scripts/dfn3/convert-weights.py` in Sew's repository, converter version 1, script sha256 `8b5384c27c63e2541763d1f7a4ba570f9a3146c2b65dd6f0032dbac95715ff75`; Python standard library only |
| Licence | MIT OR Apache-2.0 (the upstream repository's `LICENSE`, `LICENSE-MIT`, `LICENSE-APACHE`) |

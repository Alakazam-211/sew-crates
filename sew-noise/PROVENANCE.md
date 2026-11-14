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

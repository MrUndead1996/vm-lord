# Dependency licence audit

The dependency graph recorded in `Cargo.lock` was audited on 28 August 2026
before VMLord was relicensed under `GPL-3.0-or-later`, and re-audited on
4 September 2026 after the `eframe`, `rust-i18n` and `arcbox-dhcp` upgrades.

## Scope

The audit covered all 612 third-party package/version entries resolved for the
workspace in `Cargo.lock`, including target-specific, build, and development
dependencies. Every registry package declared a licence or a licence file in
its package metadata.

The dependency expressions permit GPLv3 distribution. Most dependencies offer
MIT, Apache-2.0, BSD, ISC, Zlib, or similarly permissive terms. Expressions
which also offer LGPL or GPL alternatives -- `r-efi` and `self_cell` -- have a
permissive choice, and `about.toml` accepts only that half. The remaining data
and asset licences in the graph are `OFL-1.1`, `Ubuntu-font-1.0`,
`Unicode-3.0`, `Unicode-DFS-2016`, and `CDLA-Permissive-2.0`; none prevents
GPLv3 distribution of VMLord.

The guest payload sources keep their own licences. Their manifests already map
the Linux module, Linux headers, Mesa, and DirectX Headers to the corresponding
licence texts under `payloads/**/licenses/`. Relicensing VMLord does not change
those upstream terms.

## Repeating the audit

From the workspace root, list the selected packages and their declared licence
expressions with:

```sh
cargo metadata --locked --format-version 1 \
  | jq -r '.packages[] | select(.source != null) |
      [.name, .version, (.license // ("FILE:" + (.license_file // "MISSING")))] |
      @tsv' \
  | sort -k3,3 -k1,1
```

Repeat the audit whenever `Cargo.lock` gains or upgrades a dependency. Package
metadata is an inventory aid rather than a substitute for reading an unfamiliar
licence or its exceptions.

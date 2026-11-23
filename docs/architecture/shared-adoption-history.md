# Shared adoption history

<!-- One row per `python3 scripts/shared_adoption_index.py --record`. -->

Each product cell is `SAI v2 · dup%`: the Shared Adoption Index (0–100,
higher means more of the product is built on `shared/`) and the share of the
product's production lines that are clones of another product's code (lower
means less logic that can drift). The last column is the cloned production
lines shared by the Linux and Windows Piers. How each number is computed is documented in
`scripts/shared_adoption_index.py`; the gate on differences is
[platform-differences.md](platform-differences.md).

| Date | Version | Linux Pier | Windows Pier | macOS Pier | macOS Deck | Windows Deck | capenc | Linux↔Windows clones |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 2026-11-15 | 0.19.1 | 75 · 4.4 % | 62 · 5.4 % | 74 · 0.4 % | 60 · 0.1 % | 64 · 0.1 % | 69 · 0.1 % | 2,258 |
| 2026-11-15 | 0.19.1 | 75 · 4.4 % | 62 · 5.4 % | 74 · 0.4 % | 60 · 0.1 % | 64 · 0.1 % | 69 · 0.1 % | 2,258 |
| 2026-11-16 | 0.19.1 | 76 · 2.7 % | 65 · 3.4 % | 74 · 0.4 % | 60 · 0.1 % | 64 · 0.1 % | 70 · 0.1 % | 1,353 |
| 2026-11-22 | 0.20.8 | 76 · 2.2 % | 70 · 3.0 % | 74 · 0.4 % | 65 · 0.1 % | 66 · 0.1 % | 72 · 0.1 % | 1,284 |
| 2026-11-23 | 0.20.8 build 2 | 76 · 2.2 % | 70 · 3.0 % | 74 · 0.4 % | 65 · 0.1 % | 66 · 0.1 % | 72 · 0.1 % | 1,284 |

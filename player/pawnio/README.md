# PawnIO modules

**These files are not under this repo's MIT licence.** They're namazso's PawnIO modules, under the GNU
Lesser General Public License, version 2.1 or later (LGPL-2.1-or-later): see [COPYING](COPYING). The
headers in `src/include/` are 0BSD, as each one says at its top.

The player reads PC sensors through them, with the PawnIO driver (pawnio.eu), which you install yourself:

| File | What it reads |
|---|---|
| `AMDFamily17.bin` | AMD CPUs of families 17h to 1Ah (Zen 1 to Zen 5): temperatures and package power |
| `LpcIO.bin` | the motherboard's Super I/O sensor chips: fan and pump speeds, and board temperatures |

## Where they're from

[namazso/PawnIO.Modules](https://github.com/namazso/PawnIO.Modules), release
[0.2.11](https://github.com/namazso/PawnIO.Modules/releases/tag/0.2.11), tag commit
`52a7e536dff3e53c96917a28caac5e0fa6510696`. **All unmodified:**

- `AMDFamily17.bin`, `LpcIO.bin` and `COPYING` are from the release's `release_0_2_11.zip` (SHA-256
  `43608cb89bc84247fef1368a139013f7d043e17db6d6c8dfc9b46bf0905a81f4`, as GitHub lists it).
- `src/` is their source at that commit: `AMDFamily17.p`, `LpcIO.p`, and in `src/include/` the six headers
  they include.

| File | SHA-256 |
|---|---|
| `AMDFamily17.bin` | `dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91` |
| `LpcIO.bin` | `b3896a1cab0d808fca31fe2ebcae045d59dac690da87b17c858bb8da357eb45e` |
| `COPYING` | `1e7e6bae5a5bde32f1ae5a7c37a082d1ab03cf89354f7f936ac40be9e39a6531` |

The player pins the two modules' values, and refuses a module file that doesn't match.

## Building them

The project builds each module with pawncc 4.1.7152 (its `.github/workflows/ci.yml`). From `src/`:

```
pawncc AMDFamily17.p -iinclude -C64 '-;+' '-(+' -p
pawncc LpcIO.p -iinclude -C64 '-;+' '-(+' -p
```

Each gives an `.amx` file. A `.bin` here is that AMX signed by namazso: a 4-byte length (512, little-endian),
the 512-byte signature, then the AMX. The PawnIO driver loads only modules signed with namazso's key, so a
module you rebuild or change won't load in the official driver. Nobody here has rebuilt these to check that
the AMX comes out byte for byte.

## Keeping them

- Keep this folder whole: wherever the `.bin` files go, `COPYING` and `src/` go too (LGPL-2.1 sections 1
  and 4).
- Keep them separate files that the player reads when it runs. Don't build them into `wvplay.exe` (with
  `include_bytes!`, say): a program that contains them is covered by LGPL-2.1 section 6, not section 5.
- To move to another release, replace the modules, `COPYING` and `src/` together, then change the values
  above and the player's pins (`player/src/pawnio/blobs.rs`).
- `.gitattributes` has Git keep every file here byte for byte in any checkout (`COPYING` has CRLF line
  endings and the sources LF), so the values above stay true.

# Embedded UI fonts

`NotoSansSC-Regular-subset.otf` backs CJK text in the UI (track names, clip
names, imported file names). It is embedded at compile time by `src/fonts.rs`
via `include_bytes!` and registered as a **lowest-priority** fallback, so Latin
text still renders with egui's bundled faces.

## Provenance

| Item | Value |
| :--- | :--- |
| Upstream | Noto Sans SC Regular (Noto CJK, Adobe/Google) |
| Source file | `Sans/SubsetOTF/SC/NotoSansSC-Regular.otf`, 8,331,336 bytes |
| Mirror | <https://cdn.jsdelivr.net/gh/notofonts/noto-cjk@main/Sans/SubsetOTF/SC/> (the TUNA mirror carries the same design under `adobe-fonts/source-han-sans`) |
| License | SIL Open Font License 1.1 — `OFL.txt`, shipped with the binary as `OFL-NotoSansSC.txt` |
| Derived | yes: glyph subset, CFF hints stripped |
| Result | 23,187 glyphs, 5,209,528 bytes |

## Coverage

| Block | Included |
| :--- | :--- |
| Basic Latin, Latin-1, General Punctuation, Currency, Letterlike, Arrows, Math Operators, Geometric Shapes | yes |
| CJK Symbols and Punctuation, Hiragana/Katakana, Halfwidth/Fullwidth Forms | yes |
| CJK Unified Ideographs (U+4E00–U+9FFF) | **all 20,976 mapped code points** |
| CJK Compatibility Ideographs (U+F900–FAFF), Small Form punctuation (U+FE50–FE6B) | yes |
| Roman numerals, enclosed alphanumerics (①–⑩, ⑴–⒇, ㈠–㈩), box drawing, block elements, Bopomofo (ㄅ–ㄩ), CJK compatibility units (㎎, ㎜, ㊣) | yes |
| IPA/modifier letters (ɑ, ɡ, ˇ ˊ) | yes |
| Greek, Cyrillic, Latin Extended-A (pinyin ā ǎ), ★☆☉♀♂ | no — supplied by egui's `Ubuntu-Light` / `NotoEmoji` earlier in the chain |
| `≒` (U+2252), `⌒` (U+2312) | no — absent from the upstream face, remains tofu |
| CJK Extension A/B, Hangul, emoji | no |

Net: **every GBK character renders** except `≒` and `⌒`, and the ~5,000
base-block ideographs GBK lacks (玥, 喆, 堃, 龘 …) render as well.

## Why not Source Han Sans CN

Same typeface, same metrics (both trace back to the Adobe/Google CJK project).
The Source Han family name carries Adobe's Reserved Font Name "Source", and a
subset is a *Modified Version* under OFL §3 — it could not keep that name when
redistributed without renaming the `name` table. "Noto Sans SC" carries no
reserved name, so the subset ships as-is.

## Regenerating

Requires Python 3 with `fonttools` (`pip install fonttools brotli`).

```bash
python -m fontTools.subset NotoSansSC-Regular.otf \
    --unicodes="0020-007E,00A0-00FF,0250-02FF,2000-206F,20A0-20BF,2100-214F,2160-217F,2190-21FF,2200-22FF,2460-24FF,2500-259F,25A0-25FF,3000-303F,3040-30FF,3105-3129,3200-33FF,4E00-9FFF,F900-FAFF,FE30-FE4F,FE50-FE6B,FF00-FFEF" \
    --output-file=NotoSansSC-Regular-subset.otf \
    --no-hinting
```

`--no-hinting` is safe: epaint rasterizes through `skrifa`, which ignores CFF
hinting. Full file 8.3 MB → subset with hints 6.3 MB → hints stripped 4.97 MB.

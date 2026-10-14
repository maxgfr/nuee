# Optical Recognition of Handwritten Digits (optdigits)

`optdigits.tra` (3,823 training samples) and `optdigits.tes` (1,797 test samples) are the
original files of the UCI Machine Learning Repository dataset, unmodified.

- Authors: E. Alpaydin and C. Kaynak (1998).
- Citation: Alpaydin, E. & Kaynak, C. (1998). Optical Recognition of Handwritten Digits
  [Dataset]. UCI Machine Learning Repository. https://doi.org/10.24432/C50P49
- Source: https://archive.ics.uci.edu/dataset/80/optical+recognition+of+handwritten+digits
- License: Creative Commons Attribution 4.0 International (CC BY 4.0),
  https://creativecommons.org/licenses/by/4.0/

Each row holds 64 integers in 0..=16 (pixel counts of the 4×4 blocks of a 32×32 bitmap, an 8×8
image) followed by the class 0..=9.

`scripts/build-tasks.sh` packs both files into `optdigits.bin` (the toy trainer's dataset asset,
copied to `target/tasks/` and `web/tasks/`): `b"OPTD"`, `u32 LE n_train`, `u32 LE n_test`, then
65 bytes per sample (64 pixels, then the label), training samples first.

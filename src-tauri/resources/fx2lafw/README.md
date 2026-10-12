# FX2 logic analyzer firmware

Unmodified files from sigrok-firmware-fx2lafw 0.1.7, used by `hsync-fx2lafw`.

- Binary archive: https://sigrok.org/download/binary/sigrok-firmware-fx2lafw/sigrok-firmware-fx2lafw-bin-0.1.7.tar.gz
- Corresponding source: https://sigrok.org/download/source/sigrok-firmware-fx2lafw/sigrok-firmware-fx2lafw-0.1.7.tar.gz
- License: GPL-2.0-or-later; fx2lib retains its upstream LGPL notices. See `COPYING`, `COPYING.LESSER`, `UPSTREAM-README` and the bundled source archive.
- The source archive includes fx2lib and the upstream build instructions. It ships alongside the firmware in installers.

Only the Saleae Logic compatible and sigrok eight-channel images are bundled. Uploads target volatile FX2 RAM, never EEPROM. Existing DSLogic and PXLogic resources are unchanged.

## SHA-256

```text
dbb9fc37e9cceaa1034f6f68d99d752e0570f449b3a6c1b7dec45df28e614863  fx2lafw-saleae-logic.fw
b667d878d5455f854bd912704c68cc2cf25702032e72ff825393409890a86e37  fx2lafw-sigrok-fx2-8ch.fw
a3f440d6a852a46e2c5d199fc1c8e4dacd006bc04e0d5576298ee55d056ace3b  sigrok-firmware-fx2lafw-0.1.7.tar.gz
```

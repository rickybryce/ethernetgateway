# Manual screenshots

How the images in `manual-images/` were made (2026-11-22, from the v1.0.8
x86_64 build), so they can be made again when the screens change.  Everything
runs against a **scratch** gateway in `~/manual-shots/main` (its own
`ethernetgateway-data/`, default ports), never a real installation.

| Image | How |
|---|---|
| `gui-main-window.png` | `import -window <id>` of the gateway's own window (`xwininfo -root -tree`) |
| `gui-server-more.png` | `click.py <id> 605 77` (Server *More…*), capture, crop to the popup |
| `gui-setup-wizard.png` | a second scratch dir whose config says `setup_wizard_completed = false` (and other ports); top 405 px |
| `web-config.png` | headless Chrome on the live `http://127.0.0.1:8080/` (so the log panel fills in), trimmed + 16 px margin |
| `web-mount-cpm-drives.png` | a saved copy of the page with `<base>` and a script that clicks `data-target="more-cpm-disks"`; cropped to the modal |
| `telnet-*.png` | `shoot-telnet.sh <name> <steps>`: `term.py` in an 80x24 xterm (DejaVu Sans Mono 14), ANSI + colour |

Telnet steps used: `main w:1` · `xfer s:F w:2` · `shell s:F w:1.5 s:S w:1.5 's:DIR\r' w:2` ·
`cpm s:K w:4 's:DIR B:\r' w:3` · `browser s:B w:3 's:gopher://gopher.floodgap.com\r' w:10` ·
`weather s:W w:3 's:\r' w:10`.  The scratch gateway had `weather_location = Dallas`,
four Altair8800 sample disks in `CPM/images` with `cpm_mounts = B=TDISK01.DSK`, and
README/LICENSE/PUNTEST.SEQ plus an `UPLOADS/` folder in the transfer directory.

Every image is then reduced with `convert in.png -strip -colors 256 PNG8:out.png`
(measured: under 0.5 % RMSE, a third to a quarter of the size).

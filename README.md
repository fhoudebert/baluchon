# 💾 Baluchon – Gestionnaire d'applications portables

Interface graphique multiplateforme (Linux / Windows) pour gérer les applications portables d'une clé USB.

---

## Features

| Feature                                               | Linux                            | Windows                 |
| ----------------------------------------------------- | -------------------------------- | ----------------------- |
| Automatic USB drive detection                         | ✅ `/media`, `/run/media`, `/mnt` | ✅ Drive letters D: → Z: |
| `apps.json` parsing                                   | ✅                                | ✅                       |
| Launch an application                                 | ✅                                | ✅                       |
| Install a shortcut                                    | ✅ `.desktop`                     | ✅ `.lnk` (PowerShell)   |
| Remove a shortcut                                     | ✅                                | ✅                       |
| Open download URL                                     | ✅ `xdg-open`                     | ✅ `start`               |
| Desktop environment detection (GNOME, KDE, Cinnamon…) | ✅                                | —                       |
| Desktop shortcut creation                             | ✅                                | —                       |
| `.desktop` database update                            | ✅ `update-desktop-database`      | —                       |
| FR/EN internationalization                            | ✅                                | ✅                       |


---

## `apps.json` Format


Place this file at **the root of the USB drive**:

```json
[
  {
    "name": "MonApp",
    "exec_linux": "monapp/monapp",
    "exec_windows": "monapp/monapp.exe",
    "icon": "monapp/assets/icon.png",
    "description": "Description courte",
    "setup_script": "monapp/setupPython_and_download.sh"
  }
]
```

All fields except name are optional.

---

## Compilation


### Compiler

```sh
# Linux – native binary
cargo build --release
# → ./target/release/baluchon

# Windows (cross-compilation from Linux)
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
# → ./target/x86_64-pc-windows-gnu/release/baluchon.exe
```

## USB Drive



### USB drive detection

1. Walks up to 4 levels up from the executable looking for apps.json
2.Scans /media/$USER, /run/media/$USER, /mnt (Linux)
3.Scans drive letters D: → Z: (Windows)

## Technical details

### Linux shortcuts (.desktop)

- Created in ~/.local/share/applications/
- Also copied to ~/Desktop for GNOME, KDE, Cinnamon, XFCE
- Automatically applies chmod 755
- Runs update-desktop-database after installation
### Windows shortcuts (`.lnk`)

Created in %APPDATA%\Microsoft\Windows\Start Menu\Programs\ via an embedded PowerShell script

### Internationalisation

Language is detected from $LANG / $LANGUAGE. It can be switched at runtime in the interface (FR / EN buttons).

### dependencies
| Crate                  | Role                                             |
| ---------------------- | ------------------------------------------------ |
| `eframe` + `egui`      | Native GUI (OpenGL)                              |
| `serde` + `serde_json` | JSON parsing                                     |
| `dirs-next`            | Cross-platform user paths (`~/.local`, desktop…) |
---


## Screenshots

### GUI
![Home](images/baluchon.png)

## Licence

MIT

# BDEngine App

Windows desktop shell for BDEngine.

This repository contains the desktop wrapper source code and installer script.
It does not include the full web editor source code.

## Features

- Windows desktop launcher for `bdengine.app`
- `.bdengine` file association
- `bdengine://` deep link support
- custom Windows file icon integration
- remembered editor window size, position, and maximized/fullscreen state

## Notes

- Microsoft Edge WebView2 Runtime is required
- if it is missing, the app will show a prompt and open the official Microsoft download page

## Releases

Prebuilt Windows installers are published in GitHub Releases.

## Building on Windows

Requires Node.js with npm, Rust with the MSVC toolchain, and Visual Studio C++
Build Tools with a Windows SDK.

```sh
npm ci
npm run build
```

The application is prepared in `build/app`. Open `installer/bdengine.iss` in
Inno Setup to build the installer.

## License

BDEngine App is free software: you can redistribute it and/or modify it under
the terms of the GNU General Public License as published by the Free Software
Foundation, version 3 only (`GPL-3.0-only`).

It is distributed without any warranty. See [LICENSE](LICENSE) for details.
Third-party dependencies remain under their respective licenses.

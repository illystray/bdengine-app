Optional Microsoft WebView2 installers for the application build.

npm run build copies one available installer to build/app/redist:
- MicrosoftEdgeWebView2RuntimeInstallerX64.exe (preferred offline installer)
- MicrosoftEdgeWebview2Setup.exe (bootstrapper fallback)

Download the official bootstrapper with npm run fetch:webview2.
Copying an installer here does not automatically run it or include it in Inno Setup.

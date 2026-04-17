#define AppName "BDEngine"
#define AppVersion "1.0.0"
#define AppPublisher "illystray Creations"
#define AppExeName "bdengine_app.exe"
#define AppAssocName "BDEngine File"
#define AppAssocExt ".bdengine"
#define AppAssocKey StringChange(AppAssocName, " ", "") + AppAssocExt
#define AppProtocol "bdengine"

[Setup]
AppId={{A61FD08D-6E12-4A25-BD57-2D252B7FEE01}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
UninstallDisplayIcon={app}\{#AppExeName}
OutputDir=..\build\inno
OutputBaseFilename=BDEngineSetup
SetupIconFile=..\src-tauri\icons\icon.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
ChangesAssociations=yes
DisableDirPage=no
DisableProgramGroupPage=yes

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"

[Files]
Source: "..\build\steam\content\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\build\steam\content\fileicons\bdengine-file.ico"; DestDir: "{app}\fileicons"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Classes\{#AppProtocol}"; ValueType: string; ValueName: ""; ValueData: "URL:{#AppName} Protocol"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\{#AppProtocol}"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\{#AppProtocol}\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#AppExeName},0"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\{#AppProtocol}\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExeName}"" ""%1"""; Flags: uninsdeletekey

Root: HKCU; Subkey: "Software\Classes\{#AppAssocExt}"; ValueType: string; ValueName: ""; ValueData: "{#AppAssocKey}"; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\{#AppAssocKey}"; ValueType: string; ValueName: ""; ValueData: "{#AppAssocName}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\{#AppAssocKey}\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\fileicons\bdengine-file.ico"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\{#AppAssocKey}\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExeName}"" ""%1"""; Flags: uninsdeletekey

[Run]
Filename: "{app}\{#AppExeName}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent

[Code]
procedure SHChangeNotify(wEventId: Cardinal; uFlags: Cardinal; dwItem1: Integer; dwItem2: Integer);
  external 'SHChangeNotify@shell32.dll stdcall';

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    SHChangeNotify($08000000, $0000, 0, 0);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    SHChangeNotify($08000000, $0000, 0, 0);
  end;
end;

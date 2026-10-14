; Widget Panel — Windows installer (Inno Setup 6). Per-user install, no admin needed.
#define AppName "Widget Panel"
#ifndef AppVersion
  #define AppVersion "1.0.0"
#endif
#define AppExe "WidgetPanel.exe"

[Setup]
AppId={{6C0B4F53-8E0B-4E0F-9D5B-2E5A1B7C3D11}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Nightmare
DefaultDirName={localappdata}\Programs\Widget Panel
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
MinVersion=10.0
UninstallDisplayName={#AppName}
OutputDir=..\dist
OutputBaseFilename=WidgetPanel-Setup
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\{#AppExe}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=force
RestartApplications=no

[Tasks]
Name: "startup"; Description: "Start Widget Panel when Windows starts"; GroupDescription: "Options:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Options:"; Flags: unchecked
Name: "contextmenu"; Description: "Add ""Add to Widget Panel"" to the right-click menu"; GroupDescription: "Options:"

[Files]
Source: "..\target\release\widget-panel.exe"; DestDir: "{app}"; DestName: "{#AppExe}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{userdesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "RightPanel"; ValueData: """{app}\{#AppExe}"""; Flags: uninsdeletevalue; Tasks: startup

[Run]
Filename: "{app}\{#AppExe}"; Parameters: "--context-menu on"; Flags: runhidden waituntilterminated; Tasks: contextmenu
Filename: "{app}\{#AppExe}"; Description: "Launch Widget Panel now"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\{#AppExe}"; Parameters: "--context-menu off"; Flags: runhidden waituntilterminated; RunOnceId: "CtxMenu"
Filename: "taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "KillApp"

[UninstallDelete]
Type: filesandordirs; Name: "{userappdata}\RightPanel\webview"

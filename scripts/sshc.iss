; Instalador Windows do sshc (Inno Setup 6).
#ifndef MyAppVersion
  #define MyAppVersion "0.0.0"
#endif

[Setup]
AppId=dev.sshc.client
AppName=sshc
AppVersion={#MyAppVersion}
AppPublisher=sshc
AppPublisherURL=https://github.com/diorgesl/sshc
AppSupportURL=https://github.com/diorgesl/sshc
AppUpdatesURL=https://github.com/diorgesl/sshc/releases
DefaultDirName={autopf}\sshc
DefaultGroupName=sshc
DisableProgramGroupPage=yes
OutputDir=..\dist
OutputBaseFilename=sshc-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
UninstallDisplayIcon={app}\sshc-gui.exe
LicenseFile=..\LICENSE

[Languages]
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"

[Tasks]
Name: "desktopicon"; Description: "Criar atalho na Area de Trabalho"; GroupDescription: "Atalhos:"; Flags: unchecked

[Files]
Source: "..\target\release\sshc-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\sshc.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\sshc"; Filename: "{app}\sshc-gui.exe"
Name: "{group}\Desinstalar sshc"; Filename: "{uninstallexe}"
Name: "{autodesktop}\sshc"; Filename: "{app}\sshc-gui.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\sshc-gui.exe"; Description: "Abrir sshc"; Flags: nowait postinstall skipifsilent

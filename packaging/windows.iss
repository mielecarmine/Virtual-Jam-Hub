#ifndef AppVersion
  #error AppVersion is required
#endif
[Setup]
AppId={{A6EB90A8-D897-431A-8073-359E9D891DC7}
AppName=Virtual Jam
AppVersion={#AppVersion}
AppPublisher=Carmine Miele
DefaultDirName={localappdata}\Programs\Virtual Jam
DefaultGroupName=Virtual Jam
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=Virtual-Jam-{#AppVersion}-windows-x64-setup
Compression=lzma2
SolidCompression=yes
LicenseFile=..\LICENSE
UninstallDisplayIcon={app}\jam-client.exe
[Files]
Source: "..\target\release\jam-client.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
[Icons]
Name: "{group}\Virtual Jam"; Filename: "{app}\jam-client.exe"
[Run]
Filename: "{app}\jam-client.exe"; Description: "Launch Virtual Jam"; Flags: nowait postinstall skipifsilent

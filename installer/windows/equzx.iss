; EQUZX — Windows installer (Inno Setup 6)
;
; Build with:
;   iscc /DVersion=2026.8.12 /DBundled=..\..\target\bundled installer\windows\equzx.iss
;
; Both formats are directory bundles, not single files: a VST3 on Windows is
; EQUZX.vst3\Contents\x86_64-win\EQUZX.vst3, and the outer .vst3 is a folder.
; That is why the file entries below recurse rather than naming one binary.

#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef Bundled
  #define Bundled "..\..\target\bundled"
#endif

#define AppName "EQUZX"
#define Publisher "Futureboard Digital Technologies"
#define AppUrl "https://futureboard.digital"

[Setup]
AppId={{7C4E1B90-2F5A-4C3E-9E1D-EQUZXFB00001}
AppName={#AppName}
AppVersion={#Version}
AppVerName={#AppName} {#Version}
AppPublisher={#Publisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}
VersionInfoVersion={#Version}
DefaultDirName={commoncf}\VST3
DisableDirPage=yes
DisableProgramGroupPage=yes
; The plug-in folders live under Program Files, so this needs elevation.
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..\target\installer
OutputBaseFilename=EQUZX-{#Version}-windows
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName={#AppName} {#Version}
UninstallDisplayIcon={commoncf}\VST3\EQUZX.vst3\Contents\x86_64-win\EQUZX.vst3

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Types]
Name: "full"; Description: "Both formats"
Name: "custom"; Description: "Choose formats"; Flags: iscustom

[Components]
Name: "vst3"; Description: "VST3 plug-in"; Types: full custom
Name: "clap"; Description: "CLAP plug-in"; Types: full custom

[Files]
Source: "{#Bundled}\EQUZX.vst3\*"; DestDir: "{commoncf}\VST3\EQUZX.vst3"; \
  Flags: ignoreversion recursesubdirs createallsubdirs; Components: vst3
Source: "{#Bundled}\EQUZX.clap"; DestDir: "{commoncf}\CLAP"; \
  Flags: ignoreversion; Components: clap

[UninstallDelete]
; The bundle directory itself is ours, so take it with us.
Type: filesandordirs; Name: "{commoncf}\VST3\EQUZX.vst3"

[Messages]
; A DAW with the plug-in loaded holds the binary open, and the copy then fails
; with a message about a file in use that doesn't say what to do about it.
SetupAppRunningError=Close your DAW before installing {#AppName}: a host with the plug-in loaded keeps its files locked.

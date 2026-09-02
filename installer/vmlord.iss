; VMLord's Windows installer.
;
; Declarative packaging only: this script places files, offers shortcuts and
; registers an uninstaller. Every decision about settings, distribution
; profiles and updates belongs to the application, which is why nothing here
; writes or reads settings.toml.
;
; Compile from the repository root, after `cargo dist` has staged the payload.
; The version is not written here: it is passed in, and it comes from the one
; place that states it, `[workspace.package] version` in Cargo.toml.
;
;     powershell -File installer\check.ps1 target\dist
;     $v = (cargo metadata --locked --format-version 1 --no-deps |
;         ConvertFrom-Json).packages |
;         Where-Object { $_.name -eq 'vmlord' } |
;         Select-Object -ExpandProperty version
;     iscc /DAppVersion=$v installer\vmlord.iss

#define AppName "VMLord"
#define AppPublisher "VMLord contributors"
#define AppUrl "https://github.com/MrUndead1996/vm-lord"
#define AppExe "vmlord.exe"
; The version is the caller's to supply, because there is exactly one place
; that states it -- `[workspace.package] version` in Cargo.toml -- and a copy
; here would be a second. It used to be a copy, kept in step by hand, and it
; drifted: 0.2.0 was tagged and built as `VMLord-0.1.0-x86_64-setup.exe`, which
; `cargo release-manifest` then could not find. Refusing to compile is the
; cheap failure; the expensive one is an installer named after the wrong
; release, found after the whole distribution has been built.
#ifndef AppVersion
  #error "AppVersion was not passed in. Compile with /DAppVersion=<workspace version>; see the header of this file."
#endif
#define DistDir "..\target\dist"

[Setup]
; Never regenerate this. It is what lets a later version find the installation
; this one made -- including the scope it was installed in -- instead of
; landing beside it as a second copy.
AppId={{B0E4B7C1-6F49-4F2B-9A2B-7C1D5E8F3A64}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}/issues
AppUpdatesURL={#AppUrl}/releases
VersionInfoVersion={#AppVersion}
SetupIconFile=..\assets\vmlord.ico

; Both installation modes. `lowest` means the setup program does not ask for
; elevation on its own; `dialog` gives the user the choice between installing
; for themselves and for every user, and elevates only if they pick the latter.
; VMLord's own manifest asks for elevation when the application starts, so a
; per-user installation is still able to manage Hyper-V.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog

; `auto*` constants follow whichever mode was chosen: Program Files and the
; all-users Start Menu when elevated, the user's own directories when not.
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
UsePreviousAppDir=yes
DisableProgramGroupPage=yes

; Hyper-V is 64-bit only, and so is everything staged under target\dist.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

LicenseFile={#DistDir}\LICENSE
OutputDir=..\target\installer
OutputBaseFilename={#AppName}-{#AppVersion}-x86_64-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#AppExe}

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; The whole staged tree, recursively: the binaries, LICENSE, the third-party
; notices, distros\*.json, and the GPU and display payload directories when
; `cargo dist` was given them. Listing it as one entry is what keeps this
; script from needing an edit every time a payload is added.
Source: "{#DistDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
; The final page's offer. `nowait` so the wizard closes rather than waiting for
; a program the user is about to configure; `skipifsilent` so an unattended
; install stays unattended.
Filename: "{app}\{#AppExe}"; Description: "Launch VMLord and configure it"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Only what the installer itself put here. `{localappdata}\VMLord` holds the
; user's settings, VMs, images and distribution profiles, and uninstalling
; VMLord is not a request to delete them -- there is no entry for it here, and
; there must never be one.
Type: filesandordirs; Name: "{app}\distros"
Type: dirifempty; Name: "{app}"

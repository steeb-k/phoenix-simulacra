; Phoenix Simulacra installer (Inno Setup 6.6.0+).
;
; Produces a single Simulacra-Setup-<ver>.exe that lays down the dual-arch bundle
; (both x64 and ARM64 binaries + the arch-selecting launcher), silently installs
; the bundled WinFsp MSI so mounting works out of the box, and offers an opt-in
; desktop icon. See scripts/build-installer.ps1 for how this is compiled (it
; passes AppVersion and stages the WinFsp MSI under installer/build/).
;
; Requires Inno Setup 6.6.0+ for two features used below:
;   - native dark mode via WizardStyle=... dynamic  (6.6.0)
;   - PNG WizardImageFile                            (6.5.2)

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

#define AppName "Phoenix Simulacra"
#define AppPublisher "Phoenix Simulacra"
#define LauncherExe "simulacra-launcher.exe"
; WinFsp 2.1.25156 ProductCode (matches the pinned MSI in build-installer.ps1).
; Used by the uninstaller's opt-in "Remove WinFsp".
#define WinFspProductCode "{C79D9B29-3AF0-45B3-9DB9-226F3C2D2204}"

[Setup]
; A stable AppId is what ties upgrades and uninstall together across releases;
; never change it once shipped.
AppId={{B2E7A1F0-3C4D-4A5E-9F80-1D2C3B4A5E6F}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableWelcomePage=no
UninstallDisplayIcon={app}\{#LauncherExe}
SetupIconFile=..\assets\phoenix-appicon.ico
OutputDir=..\dist
OutputBaseFilename=Simulacra-Setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
MinVersion=10.0
; The bundle ships only x64 + ARM64 binaries. x64compatible admits x64 and ARM64
; (ARM64 runs the x64/x86 setup under emulation) and excludes pure x86 machines.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; WinFsp MSI install + Program Files both need elevation.
PrivilegesRequired=admin
; Let an in-place update close a running instance (needs the app-side named
; mutex "PhoenixSimulacra"; harmless if absent).
CloseApplications=yes
AppMutex=PhoenixSimulacra
; Silent updates keep the user's earlier task choices (desktop icon, WinFsp).
UsePreviousTasks=yes
; Native theming (Inno 6.6.0+): modern layout, follows the Windows light/dark
; system setting to match the theme-aware GUI. Auto-disables under high-contrast;
; /NOSTYLE opts out. Silent installs render no UI, so this never affects updates.
WizardStyle=modern dynamic
WizardImageFile=..\assets\wizardImage.png
WizardSmallImageFile=..\assets\phoenix-appicon-128px.png

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
; Both default-checked (no "unchecked" flag). The WinFsp task only appears when
; WinFsp isn't already installed.
Name: "winfsp"; Description: "Install WinFsp (*RECOMMENDED* - Required for mounting backups)"; Check: IsWinFspMissing
Name: "desktopicon"; Description: "Create a &desktop icon"

[Files]
Source: "..\dist\simulacra\*.exe"; DestDir: "{app}"; Flags: ignoreversion
; Staged WinFsp MSI, extracted only when we're actually going to install it.
Source: "build\winfsp.msi"; DestDir: "{tmp}"; Flags: deleteafterinstall; Check: ShouldInstallWinFsp

[Icons]
; Two Start Menu entries side by side: normal + debug (console). Both target the
; arch-selecting launcher; --debug is forwarded through to the GUI.
Name: "{group}\{#AppName}"; Filename: "{app}\{#LauncherExe}"
Name: "{group}\{#AppName} (Debug)"; Filename: "{app}\{#LauncherExe}"; Parameters: "--debug"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#LauncherExe}"; Tasks: desktopicon

[Run]
; Silent WinFsp install (installer is already elevated).
Filename: "msiexec.exe"; Parameters: "/i ""{tmp}\winfsp.msi"" /qn /norestart"; StatusMsg: "Installing WinFsp..."; Flags: waituntilterminated; Check: ShouldInstallWinFsp
; Offer to launch after a normal (non-silent) install.
Filename: "{app}\{#LauncherExe}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent

[Registry]
; Marker so the uninstaller only offers "Remove WinFsp" when WE installed it
; (leave a WinFsp that predated us or is shared with rclone/sshfs-win). Written
; to the 64-bit view; the whole key is removed on uninstall.
Root: HKLM; Subkey: "Software\Phoenix Simulacra"; ValueType: dword; ValueName: "InstalledWinFsp"; ValueData: 1; Flags: uninsdeletekey; Check: ShouldInstallWinFsp

[Code]
var
  GRemoveSettings: Boolean;
  GRemoveWinFsp: Boolean;

{ True when neither the ARM64-style key (HKLM64 SOFTWARE\WinFsp) nor the
  x64-style key (HKLM32 SOFTWARE\WinFsp == HKLM SOFTWARE\WOW6432Node\WinFsp)
  has an InstallDir value -- i.e. WinFsp is not installed. }
function IsWinFspMissing(): Boolean;
var
  S: String;
begin
  Result := not (
    RegQueryStringValue(HKLM64, 'SOFTWARE\WinFsp', 'InstallDir', S) or
    RegQueryStringValue(HKLM32, 'SOFTWARE\WinFsp', 'InstallDir', S));
end;

{ Install WinFsp only if the user kept the task AND it isn't already present. }
function ShouldInstallWinFsp(): Boolean;
begin
  Result := WizardIsTaskSelected('winfsp') and IsWinFspMissing();
end;

{ winfsp-sys reads the literal HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir. On
  x64 the WinFsp MSI already writes there; on ARM64 it writes HKLM\SOFTWARE\WinFsp
  instead, so mirror InstallDir into the WOW6432Node view or the app can't find
  the DLL. HKLM32 SOFTWARE\WinFsp maps to that literal path. No-op on x64. }
procedure MirrorWinFspInstallDirForArm();
var
  Dir: String;
begin
  if RegQueryStringValue(HKLM64, 'SOFTWARE\WinFsp', 'InstallDir', Dir) then
    if not RegQueryStringValue(HKLM32, 'SOFTWARE\WinFsp', 'InstallDir', Dir) then
      RegWriteStringValue(HKLM32, 'SOFTWARE\WinFsp', 'InstallDir', Dir);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    MirrorWinFspInstallDirForArm();
end;

{ Custom uninstall prompt: two opt-in, default-OFF checkboxes. "Remove WinFsp"
  is enabled only when our install marker is present. Skipped under silent
  uninstall (both stay off -> nothing extra removed, the safe default). }
function InitializeUninstall(): Boolean;
var
  Form: TSetupForm;
  Lbl: TNewStaticText;
  ChkSettings, ChkWinFsp: TNewCheckBox;
  BtnOk, BtnCancel: TNewButton;
  WinFspWasOurs: Boolean;
  D: Cardinal;
begin
  Result := True;
  GRemoveSettings := False;
  GRemoveWinFsp := False;
  if UninstallSilent then
    Exit;

  WinFspWasOurs := RegQueryDWordValue(HKLM64, 'Software\Phoenix Simulacra', 'InstalledWinFsp', D) and (D = 1);

  Form := CreateCustomForm(ScaleX(420), ScaleY(170), False, True);
  try
    Form.Caption := 'Uninstall Phoenix Simulacra';

    Lbl := TNewStaticText.Create(Form);
    Lbl.Parent := Form;
    Lbl.Left := ScaleX(16);
    Lbl.Top := ScaleY(16);
    Lbl.Width := ScaleX(388);
    Lbl.AutoSize := False;
    Lbl.WordWrap := True;
    Lbl.Height := ScaleY(34);
    Lbl.Caption := 'Phoenix Simulacra will be removed. Choose what else to clean up:';

    ChkSettings := TNewCheckBox.Create(Form);
    ChkSettings.Parent := Form;
    ChkSettings.Left := ScaleX(16);
    ChkSettings.Top := ScaleY(58);
    ChkSettings.Width := ScaleX(388);
    ChkSettings.Caption := 'Remove settings (deletes %LOCALAPPDATA%\PhoenixSimulacra)';
    ChkSettings.Checked := False;

    ChkWinFsp := TNewCheckBox.Create(Form);
    ChkWinFsp.Parent := Form;
    ChkWinFsp.Left := ScaleX(16);
    ChkWinFsp.Top := ScaleY(86);
    ChkWinFsp.Width := ScaleX(388);
    ChkWinFsp.Checked := False;
    ChkWinFsp.Enabled := WinFspWasOurs;
    if WinFspWasOurs then
      ChkWinFsp.Caption := 'Remove WinFsp'
    else
      ChkWinFsp.Caption := 'Remove WinFsp (installed separately - will be left in place)';

    BtnOk := TNewButton.Create(Form);
    BtnOk.Parent := Form;
    BtnOk.Caption := 'Uninstall';
    BtnOk.ModalResult := mrOk;
    BtnOk.Default := True;
    BtnOk.Width := ScaleX(90);
    BtnOk.Height := ScaleY(26);
    BtnOk.Left := Form.ClientWidth - ScaleX(90 + 6 + 90 + 16);
    BtnOk.Top := Form.ClientHeight - ScaleY(26 + 14);

    BtnCancel := TNewButton.Create(Form);
    BtnCancel.Parent := Form;
    BtnCancel.Caption := 'Cancel';
    BtnCancel.ModalResult := mrCancel;
    BtnCancel.Cancel := True;
    BtnCancel.Width := ScaleX(90);
    BtnCancel.Height := ScaleY(26);
    BtnCancel.Left := Form.ClientWidth - ScaleX(90 + 16);
    BtnCancel.Top := Form.ClientHeight - ScaleY(26 + 14);

    if Form.ShowModal() = mrOk then
    begin
      GRemoveSettings := ChkSettings.Checked;
      GRemoveWinFsp := ChkWinFsp.Checked and ChkWinFsp.Enabled;
    end
    else
      Result := False;
  finally
    Form.Free();
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ResultCode: Integer;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    if GRemoveSettings then
      DelTree(ExpandConstant('{localappdata}\PhoenixSimulacra'), True, True, True);
    if GRemoveWinFsp then
      Exec('msiexec.exe', '/x {#WinFspProductCode} /qn /norestart', '',
        SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end;
end;

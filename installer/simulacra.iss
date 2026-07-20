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

; Curated QEMU payload (see scripts/build-qemu-payload.ps1). DOWNLOADED at
; install time rather than embedded: it is 84 MB, most installs are updates
; delivered by the auto-updater, and QEMU changes far more rarely than the app
; does -- so carrying it in every installer would spend that on nearly every
; update for a payload that almost never changed. Skipping it is not fatal: the
; app's Virtualize page offers the same download later.
;
; The version must be QEMU 11.1+ or host<->guest clipboard silently disappears,
; and Windows build numbers do not track upstream tags -- this build reports
; 11.0.50 and IS the 11.1 development tree. Keep in step with the pin in
; phoenix-gui/src/qemu_payload.rs. See docs/VIRTUALIZATION.md.
#define QemuVersion "11.0.50"
#define QemuZip "qemu-x86_64-11.0.50-20260501-win64.zip"
; Hosted in the separate deps repo, not the binaries repo: an asset there would
; sit alongside app releases and read as one, and the in-app updater reads that
; repo's "latest release".
#define QemuUrl "https://github.com/steeb-k/phoenix-simulacra-deps/releases/download/qemu-payload-11.0.50/qemu-x86_64-11.0.50-20260501-win64.zip"
#define QemuSha256 "c86f19d18e0b479922ea01f2a6eb91952de5ccc543960d318f4ddadf13590c8c"

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
; Required by the `extractarchive` flag on the downloaded QEMU zip. Costs about
; a megabyte of installer for the extraction support.
ArchiveExtraction=enhanced
MinVersion=10.0
; The bundle ships only x64 + ARM64 binaries. x64compatible admits x64 and ARM64
; (ARM64 runs the x64/x86 setup under emulation) and excludes pure x86 machines.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; WinFsp MSI install + Program Files both need elevation.
PrivilegesRequired=admin
; Updates are applied while the app is CLOSED (IDE/browser style: the updater
; stages the new setup and runs it silently after the user exits), so the
; installer never force-closes a running instance -- a backup in progress is
; never interrupted.
CloseApplications=no
; Silent updates keep the user's earlier task choices (desktop icon, WinFsp).
UsePreviousTasks=yes
; Native theming (Inno 6.6.0+): modern layout, follows the Windows light/dark
; system setting to match the theme-aware GUI. Auto-disables under high-contrast;
; /NOSTYLE opts out. Silent installs render no UI, so this never affects updates.
WizardStyle=modern dynamic
WizardImageFile=..\assets\wizardImage.png
WizardSmallImageFile=..\assets\phoenix-appicon-128px.png
; Under WizardStyle=... dynamic, a dark Windows theme uses the *DynamicDark image
; slots; if unset, Setup falls back to its built-in dark image and ignores the
; light WizardImageFile above. The wizard art is transparent RGBA, so it adapts
; to either theme background -- point the dark slots at the same files.
WizardImageFileDynamicDark=..\assets\wizardImage.png
WizardSmallImageFileDynamicDark=..\assets\phoenix-appicon-128px.png

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
; All default-checked (no "unchecked" flag). The WinFsp task only appears when
; WinFsp isn't already installed; the QEMU task is always offered because it
; installs privately and so never conflicts with a QEMU the user already has.
Name: "winfsp"; Description: "Install WinFsp (*RECOMMENDED* - Required for mounting backups)"; Check: IsWinFspMissing
Name: "qemu"; Description: "Download and install QEMU (*RECOMMENDED* - Required for booting backups as VMs, 84 MB download)"; Check: IsQemuSupported
Name: "desktopicon"; Description: "Create a &desktop icon"

[Files]
Source: "..\dist\simulacra\*.exe"; DestDir: "{app}"; Flags: ignoreversion
; Staged WinFsp MSI, extracted only when we're actually going to install it.
Source: "build\winfsp.msi"; DestDir: "{tmp}"; Flags: deleteafterinstall; Check: ShouldInstallWinFsp
; QEMU goes in a PRIVATE subfolder, never Program Files\qemu: a QEMU the user
; installs themselves must not be touched, overwritten or version-clashed with.
; The app looks here first and the location is user-changeable in its UI.
;
; `external` because the zip is downloaded to {tmp} during the wizard rather
; than compiled in; `extractarchive` unpacks it in place. Guarded by
; ShouldExtractQemu so a failed or skipped download is simply absent rather
; than a broken install.
Source: "{tmp}\{#QemuZip}"; DestDir: "{app}\qemu"; Flags: external extractarchive recursesubdirs ignoreversion; Check: ShouldExtractQemu

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
  { The QEMU zip actually landed in the temp folder and matched its hash.
    Only then is there anything to extract. }
  GQemuDownloaded: Boolean;
  QemuDownloadPage: TDownloadWizardPage;

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

{ The VM feature is x86_64-only -- ARM64 QEMU has no useful acceleration for
  x86 guests, so the app hides the Virtualize page there entirely. Offering a
  download that could never be used would be worse than not offering it. }
function IsQemuSupported(): Boolean;
begin
  Result := not IsArm64;
end;

{ The user asked for QEMU and this machine can use it. }
function ShouldDownloadQemu(): Boolean;
begin
  Result := IsQemuSupported() and WizardIsTaskSelected('qemu');
end;

{ ...and the download actually succeeded. Kept separate so a failed download
  skips the extraction instead of failing the whole install. }
function ShouldExtractQemu(): Boolean;
begin
  Result := GQemuDownloaded;
end;

procedure InitializeWizard();
begin
  QemuDownloadPage := CreateDownloadPage(
    'Downloading QEMU',
    'Simulacra is fetching QEMU, needed to boot backups as virtual machines.',
    nil);
  QemuDownloadPage.ShowBaseNameInsteadOfUrl := True;
end;

{ Fetch the payload between the Ready page and the install. DownloadPage.Add
  verifies the SHA-256 itself, so a truncated or substituted file fails here
  rather than producing a broken QEMU.

  A failure is deliberately NOT fatal. An offline or firewalled machine should
  still get the app; the Virtualize page offers the same download later, which
  is also the path for anyone who unticks the task. }
function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = wpReady) and ShouldDownloadQemu() then begin
    QemuDownloadPage.Clear;
    QemuDownloadPage.Add('{#QemuUrl}', '{#QemuZip}', '{#QemuSha256}');
    QemuDownloadPage.Show;
    try
      try
        QemuDownloadPage.Download;
        GQemuDownloaded := True;
      except
        GQemuDownloaded := False;
        if QemuDownloadPage.AbortedByUser then
          Log('QEMU download aborted by user; the app can fetch it later.')
        else
          SuppressibleMsgBox(
            'QEMU could not be downloaded:' + #13#10#13#10 +
            AddPeriod(GetExceptionMessage) + #13#10#13#10 +
            'Setup will continue without it. You can download QEMU later from ' +
            'the Virtualize page in Phoenix Simulacra.',
            mbInformation, MB_OK, IDOK);
      end;
    finally
      QemuDownloadPage.Hide;
    end;
  end;
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

; Compile through build.ps1 so version, architecture and staged files are checked.
#ifndef AppVersion
  #error AppVersion must be supplied by build.ps1
#endif
#ifndef PayloadDir
  #error PayloadDir must be supplied by build.ps1
#endif
#ifndef OutputDir
  #error OutputDir must be supplied by build.ps1
#endif

[Setup]
AppId={{5A586F12-8C79-4CEB-88D8-E6FC95710DD7}
AppName=Pool Router
AppVersion={#AppVersion}
AppPublisher=Pool Router contributors
AppPublisherURL=https://github.com/qzkinhit/codex-wiretap
AppSupportURL=https://github.com/qzkinhit/codex-wiretap/issues
AppUpdatesURL=https://github.com/qzkinhit/codex-wiretap/releases
DefaultDirName={localappdata}\Programs\Pool Router
DisableDirPage=yes
DefaultGroupName=Pool Router
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename=PoolRouter-Windows-x64-Setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
LicenseFile={#PayloadDir}\LICENSE.txt
UninstallDisplayIcon={app}\pool-router-launcher.exe
UninstallDisplayName=Pool Router
CloseApplications=no
RestartApplications=no
SetupLogging=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked
Name: "startup"; Description: "Start Pool Router in the background when I sign in"; GroupDescription: "Startup:"; Flags: unchecked

[Files]
; Explicit allowlist. Never include a workspace, config, credential or log directory.
Source: "{#PayloadDir}\pool-router.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\pool-router-launcher.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#PayloadDir}\maintenance.ps1"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\maintenance.ps1"; Flags: dontcopy

[Icons]
Name: "{userprograms}\Pool Router\Pool Router"; Filename: "{app}\pool-router-launcher.exe"; WorkingDir: "{app}"
Name: "{userprograms}\Pool Router\Uninstall Pool Router"; Filename: "{uninstallexe}"
Name: "{userdesktop}\Pool Router"; Filename: "{app}\pool-router-launcher.exe"; WorkingDir: "{app}"; Tasks: desktopicon
Name: "{userstartup}\Pool Router"; Filename: "{app}\pool-router-launcher.exe"; Parameters: "--background"; WorkingDir: "{app}"; Tasks: startup

[Run]
Filename: "{app}\pool-router-launcher.exe"; Description: "Launch Pool Router and open its local dashboard"; Flags: nowait postinstall skipifsilent

[Code]
function RunMaintenance(ScriptPath, Action: String; var ExitCode: Integer): Boolean;
var
  Parameters: String;
begin
  Parameters := '-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' +
    ScriptPath + '" -Action ' + Action + ' -InstallDir "' + ExpandConstant('{app}') + '"';
  Result := Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    Parameters, '', SW_HIDE, ewWaitUntilTerminated, ExitCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ExitCode: Integer;
begin
  Result := '';
  ExtractTemporaryFile('maintenance.ps1');
  if not RunMaintenance(ExpandConstant('{tmp}\maintenance.ps1'), 'Stop', ExitCode) then
    Result := 'Could not prepare this installation. Close Pool Router and try again.'
  else if ExitCode <> 0 then
    Result := 'Could not stop this installed Pool Router safely. Close its processes and try again. Your configuration is preserved.';
end;

function InitializeUninstall(): Boolean;
var
  ExitCode: Integer;
  Detail: String;
begin
  Result := False;
  if not FileExists(ExpandConstant('{app}\maintenance.ps1')) then
    Detail := 'The uninstall helper is missing. Reinstall Pool Router before uninstalling it. All files are preserved.'
  else if not RunMaintenance(ExpandConstant('{app}\maintenance.ps1'), 'Uninstall', ExitCode) then
    Detail := 'The uninstall helper could not run. Pool Router and your configuration are preserved.'
  else if ExitCode = 20 then
    Detail := 'Your Codex or CC Switch connection could not be restored because its managed settings have changed. Uninstall was cancelled to preserve the service, configuration and recovery backups. Open Pool Router, resolve the connection conflict, then uninstall again.'
  else if ExitCode <> 0 then
    Detail := 'Pool Router could not be stopped safely. Uninstall was cancelled and your files are preserved.'
  else begin
    Result := True;
    Exit;
  end;
  Log(Detail);
  SuppressibleMsgBox(Detail, mbError, MB_OK, IDOK);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    SuppressibleMsgBox('Pool Router was removed. Your configuration, logs and recovery history remain in %LOCALAPPDATA%\Pool Router.', mbInformation, MB_OK, IDOK);
end;
